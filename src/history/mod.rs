//! Local 统计 history (analytics design §1, §5–§9). One SQLite file per
//! node (`data/history.sqlite`), one writer thread, read-only API queries.
//! Never synced, never backed up, never in `/status` or logs with secrets.

pub(crate) mod capture;
pub(crate) mod enrich;
pub mod event;
pub mod live_book;
pub(crate) mod maintain;
pub(crate) mod query;
pub(crate) mod schema;
pub(crate) mod writer;

#[cfg(test)]
mod tests;

use event::Event;
pub use event::{BiliTotals, EndReason, EventKind, Severity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const QUEUE: usize = 1024;
const BATCH_ROWS: usize = 200;

/// `missing_nodes` item (design §8, normative shape).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingNode {
    pub node_id: String,
    pub reason: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WriterStatus {
    pub state: &'static str,
    pub last_batch_at: Option<u64>,
    pub last_error: Option<String>,
}

pub(crate) struct Shared {
    pub path: PathBuf,
    pub recording_since: u64,
    pub dropped: AtomicU64,
    pub status: Mutex<WriterStatus>,
    pub cfg: Mutex<crate::config::HistoryConfig>,
}

struct Handle {
    tx: SyncSender<Event>,
    shared: Arc<Shared>,
}

static HANDLE: OnceLock<Handle> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);
/// Seq of the open relay session (0 = none), read by the sampler.
static OPEN_SEQ: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    crate::storage::now()
}

pub(crate) fn shared() -> Option<Arc<Shared>> {
    HANDLE.get().map(|h| Arc::clone(&h.shared))
}

/// Never blocks or awaits. A full queue drops and counts.
pub fn emit(kind: EventKind) {
    emit_at(now(), kind);
}

pub fn emit_at(at: u64, kind: EventKind) {
    let Some(handle) = HANDLE.get() else { return };
    match handle.tx.try_send(Event { at, kind }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
            handle.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(crate) fn boot_id() -> i64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bytes = (nanos ^ std::process::id() as u64).to_le_bytes();
    }
    (u64::from_le_bytes(bytes) >> 1) as i64
}

fn secrets_of(cfg: &crate::config::Config) -> Vec<String> {
    let mut out = vec![cfg.bililive.bili_rtmp_key.clone()];
    if let Some((_, query)) = cfg.bililive.bili_rtmp_url.split_once('?') {
        out.push(query.to_string());
    }
    out
}

/// Starts the writer thread, sampler and B站 summary sync. A no-op when
/// `history.enabled=false` or when already started. History is never
/// worth failing startup for: errors are logged once.
pub async fn start() {
    if HANDLE.get().is_some() {
        return;
    }
    let Ok(cfg) = crate::config::load_config().await else {
        return;
    };
    if !cfg.history.enabled {
        return;
    }
    let dir = match crate::storage::global() {
        Ok(store) => store.data_dir().to_path_buf(),
        Err(_) => return,
    };
    let path = dir.join("history.sqlite");
    let node = if cfg.cluster.enabled {
        cfg.cluster.node_id.clone()
    } else {
        "local".to_string()
    };
    let conn = match schema::open_rw(&path) {
        Ok(conn) => conn,
        Err(error) => {
            tracing::warn!("统计数据库无法打开，统计已停用: {error}");
            return;
        }
    };
    capture::set_enabled(cfg.history.capture);
    let shared = Arc::new(Shared {
        path,
        recording_since: now(),
        dropped: AtomicU64::new(0),
        status: Mutex::new(WriterStatus {
            state: "running",
            ..Default::default()
        }),
        cfg: Mutex::new(cfg.history.clone()),
    });
    let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
    let mut writer = writer::Writer::new(conn, &node, boot_id());
    writer.set_secrets(secrets_of(&cfg));
    if HANDLE
        .set(Handle {
            tx,
            shared: Arc::clone(&shared),
        })
        .is_err()
    {
        return;
    }
    let thread_shared = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("history-writer".into())
        .spawn(move || run_writer(writer, rx, thread_shared, true));
    if spawned.is_err() {
        set_state(&shared, "error", Some("writer thread".into()));
        return;
    }
    emit(EventKind::ProcessStarted {
        version: env!("CARGO_PKG_VERSION"),
    });
    tokio::spawn(sampler());
    tokio::spawn(summary_sync());
}

fn set_state(shared: &Shared, state: &'static str, error: Option<String>) {
    let mut s = shared.status.lock().unwrap_or_else(|e| e.into_inner());
    s.state = state;
    if error.is_some() {
        s.last_error = error;
    }
}

/// Updates writer-side secrets after a config change.
pub fn refresh_config(cfg: &crate::config::Config) {
    if let Some(shared) = shared() {
        *shared.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg.history.clone();
        capture::set_enabled(cfg.history.capture);
    }
    SECRETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(secrets_of(cfg));
}

static SECRETS: Mutex<Option<Vec<String>>> = Mutex::new(None);

pub(crate) fn run_writer(
    mut w: writer::Writer,
    rx: Receiver<Event>,
    shared: Arc<Shared>,
    publish: bool,
) {
    let mut last_hb = now();
    let mut last_maint = now();
    let mut last_pub = 0u64;
    let mut pending_pub = false;
    loop {
        let mut batch = Vec::new();
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ev) => {
                batch.push(ev);
                while batch.len() < BATCH_ROWS {
                    match rx.try_recv() {
                        Ok(ev) => batch.push(ev),
                        Err(_) => break,
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(secrets) = SECRETS.lock().unwrap_or_else(|e| e.into_inner()).take() {
            w.set_secrets(secrets);
        }
        let t = now();
        if !batch.is_empty() {
            match w.commit(&batch) {
                Ok(_) => {
                    let mut s = shared.status.lock().unwrap_or_else(|e| e.into_inner());
                    s.state = "running";
                    s.last_batch_at = Some(t);
                    pending_pub = true;
                }
                Err(error) => set_state(&shared, "error", Some(error.to_string())),
            }
        }
        // SSE `history` hint, at most one per 5 s.
        if publish && pending_pub && t >= last_pub + 5 {
            crate::webui::events::publish(crate::webui::events::HISTORY);
            last_pub = t;
            pending_pub = false;
        }
        if t >= last_hb + 30 {
            let _ = w.heartbeat(t);
            last_hb = t;
        }
        if t >= last_maint + 60 {
            let retention = shared
                .cfg
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retention
                .clone();
            if let Err(error) = maintain::tick(&w.conn, t as i64, &retention) {
                tracing::debug!("统计维护失败: {error}");
            }
            last_maint = t;
        }
    }
    set_state(&shared, "stopped", None);
}

// ------------------------------------------------------------ relay hooks

/// One relay session (design §5.4). `finish` records the reason; dropping
/// it unfinished records `aborted` (every `continue`, `?`, panic, cancel).
pub struct SessionGuard {
    seq: u64,
    done: bool,
}

impl SessionGuard {
    pub fn start(
        cfg: &crate::config::Config,
        platform: &str,
        channel_id: &str,
        channel_name: &str,
        title: Option<&str>,
    ) -> Self {
        refresh_config(cfg);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let room = cfg.bililive.room.max(0) as u64;
        let live_key = live_book::merged(cfg)
            .open_key(room)
            .map(|m| m.live_key.clone());
        let platform: &'static str = match platform {
            "TW" => "TW",
            "NC" => "NC",
            _ => "YT",
        };
        emit(EventKind::SessionStarted {
            seq,
            platform,
            channel_id: crate::logging::safe_text(channel_id, 80),
            channel_name: crate::logging::safe_text(channel_name, 80),
            title: title.map(|t| crate::logging::safe_text(t, 160)),
            live_key: live_key.clone(),
        });
        OPEN_SEQ.store(seq, Ordering::Relaxed);
        if live_key.is_none() && HANDLE.get().is_some() {
            // Read-only GetInfo fallback, once per session start, off the
            // stream path.
            let cfg = cfg.clone();
            tokio::spawn(async move {
                if let Some(key) = adopt_room_live_key(&cfg).await {
                    emit(EventKind::SessionLiveKey { seq, live_key: key });
                }
            });
        }
        Self { seq, done: false }
    }

    pub fn run_started(&self, run: u64) {
        emit(EventKind::FfmpegStarted { seq: self.seq, run });
    }

    pub fn run_exited(&self, run: u64, code: Option<i32>, success: bool, counted_failure: bool) {
        emit(EventKind::FfmpegExited {
            seq: self.seq,
            run,
            code,
            success,
            counted_failure,
            diagnostic: None,
        });
    }

    pub fn finish(mut self, reason: EndReason) {
        self.end(reason);
    }

    fn end(&mut self, reason: EndReason) {
        if !self.done {
            self.done = true;
            let _ = OPEN_SEQ.compare_exchange(self.seq, 0, Ordering::Relaxed, Ordering::Relaxed);
            emit(EventKind::SessionEnded {
                seq: self.seq,
                reason,
            });
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.end(EndReason::Aborted);
    }
}

async fn sampler() {
    loop {
        let secs = shared()
            .map(|s| s.cfg.lock().unwrap_or_else(|e| e.into_inner()).sample_secs)
            .unwrap_or(10)
            .clamp(5, 60);
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let seq = OPEN_SEQ.load(Ordering::Relaxed);
        if seq == 0 || !crate::plugins::ffmpeg::is_ffmpeg_running().await {
            continue;
        }
        let stats = crate::plugins::ffmpeg::get_ffmpeg_network_stats();
        let recent: Vec<f32> = stats
            .push_bitrate_history
            .iter()
            .rev()
            .take(secs as usize)
            .copied()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .collect();
        if recent.is_empty() {
            continue;
        }
        let min = recent.iter().copied().fold(f32::INFINITY, f32::min);
        let max = recent.iter().copied().fold(0.0, f32::max);
        let mean = recent.iter().sum::<f32>() / recent.len() as f32;
        emit(EventKind::StatsSample {
            seq,
            bitrate_kbps: (min, mean, max),
            fps: stats.push_fps,
            speed: crate::plugins::ffmpeg::get_ffmpeg_speed(),
        });
    }
}

// --------------------------------------------------------- B站 live hooks

fn local_node(cfg: &crate::config::Config) -> String {
    if cfg.cluster.enabled {
        cfg.cluster.node_id.clone()
    } else {
        "local".into()
    }
}

/// `startLive` returned `data.live_key`: into the book (and so into the
/// next heartbeat) and the local `bili_lives`.
pub fn on_bili_started(cfg: &crate::config::Config, live_key: &str) {
    if live_key.is_empty() || HANDLE.get().is_none() {
        return;
    }
    let room = cfg.bililive.room.max(0) as u64;
    live_book::record_started(room, live_key, &local_node(cfg), now());
    emit(EventKind::BiliLiveStarted {
        live_key: live_key.to_string(),
        room,
    });
}

/// GetInfo fallback: only when the book has no open key for the room.
async fn adopt_room_live_key(cfg: &crate::config::Config) -> Option<String> {
    let room = cfg.bililive.room.max(0) as u64;
    if let Some(mark) = live_book::merged(cfg).open_key(room) {
        return Some(mark.live_key.clone());
    }
    let reply = match crate::plugins::bilibili::bili_room_info(cfg).await {
        Ok(reply) => reply,
        Err(_) => {
            note_live_key_unavailable("getinfo_failed");
            return None;
        }
    };
    capture::room_info(&reply);
    let key = room_key_or_note(&reply)?;
    live_book::record_started(room, &key, &format!("getinfo:{}", local_node(cfg)), now());
    emit(EventKind::BiliLiveStarted {
        live_key: key.clone(),
        room,
    });
    Some(key)
}

/// Design §11: a missing `live_key` is logged by reason only, never with
/// any value from the reply. The session keeps `live_key` NULL, so its
/// StopLiveData totals stay NULL.
pub(crate) fn note_live_key_unavailable(reason: &str) {
    tracing::info!("live_key unavailable ({reason})");
}

/// `parse_room_live_key`, logging the miss.
pub(crate) fn room_key_or_note(reply: &serde_json::Value) -> Option<String> {
    let key = enrich::parse_room_live_key(reply);
    if key.is_none() {
        note_live_key_unavailable("getinfo_no_key");
    }
    key
}

static FETCHING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The B站 live session ended (stopLive OK, or the room went offline):
/// mark the book, then fetch StopLiveData once (+1 retry), deduped.
/// Closing the relay session never waits for this.
pub fn on_bili_stopped(cfg: &crate::config::Config) {
    if HANDLE.get().is_none() {
        return;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let room = cfg.bililive.room.max(0) as u64;
        let key = match live_book::merged(&cfg).latest(room) {
            Some(mark) if mark.stopped_at.is_none() => Some(mark.live_key.clone()),
            Some(_) => None,
            None => adopt_room_live_key(&cfg).await,
        };
        let Some(key) = key else { return };
        {
            let mut fetching = FETCHING.lock().unwrap_or_else(|e| e.into_inner());
            if fetching.contains(&key) {
                return;
            }
            fetching.push(key.clone());
            if fetching.len() > 16 {
                fetching.remove(0);
            }
        }
        live_book::set_stopped(room, &key, now());
        emit(EventKind::BiliLiveStopped {
            live_key: key.clone(),
            room,
        });
        if !cfg.history.bili_stop_summary {
            return;
        }
        let me = local_node(&cfg);
        live_book::set_summary(
            room,
            &key,
            live_book::SummaryPhase::Claimed,
            &me,
            now(),
            None,
        );
        if cfg.cluster.enabled {
            tokio::time::sleep(Duration::from_secs(
                cfg.cluster.heartbeat_interval_secs.max(1),
            ))
            .await;
            if live_book::should_back_off(&live_book::merged(&cfg), room, &key, &me) {
                return;
            }
        }
        let fetch_cfg = cfg.clone();
        let fetch_key = key.clone();
        let totals = enrich::fetch_with_retry(
            || {
                let cfg = fetch_cfg.clone();
                let key = fetch_key.clone();
                async move { crate::plugins::bilibili::bili_stop_live_data(&cfg, &key).await }
            },
            enrich::FIRST_DELAY,
            enrich::RETRY_DELAY,
        )
        .await;
        let phase = if totals.is_some() {
            live_book::SummaryPhase::Done
        } else {
            live_book::SummaryPhase::Failed
        };
        live_book::set_summary(room, &key, phase, &me, now(), totals.clone());
        emit(EventKind::BiliLiveSummary {
            live_key: key,
            totals,
        });
    });
}

/// Every 60 s: totals another node fetched (book `Done`) are joined into
/// this node's `bili_lives`, so its sessions show them too.
async fn summary_sync() {
    let mut applied: Vec<String> = Vec::new();
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let Ok(cfg) = crate::config::load_config().await else {
            continue;
        };
        if !cfg.cluster.enabled {
            continue;
        }
        let book = live_book::merged(&cfg);
        for mark in &book.entries {
            if mark.summary.phase == live_book::SummaryPhase::Done
                && mark.summary.totals.is_some()
                && !applied.contains(&mark.live_key)
            {
                applied.push(mark.live_key.clone());
                emit(EventKind::BiliLiveSummary {
                    live_key: mark.live_key.clone(),
                    totals: mark.summary.totals.clone(),
                });
            }
        }
        if applied.len() > 64 {
            applied.drain(..32);
        }
    }
}
