//! Local 统计 history (analytics design §1, §5–§9). One SQLite file per
//! node (`data/history.sqlite`), one writer thread, read-only API queries.
//! Never synced, never backed up, never in `/status` or logs with secrets.

pub(crate) mod bili;
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

use bili::summary_sync;
#[cfg(test)]
pub(crate) use bili::{note_live_key_unavailable, room_key_or_note};
pub use bili::{on_bili_started, on_bili_stopped};
use event::Event;
pub use event::{BiliTotals, EndReason, EventKind, Severity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, oneshot};

const QUEUE: usize = 1024;
const BATCH_ROWS: usize = 200;
// Keep crash duration within 15 s even between runs or without bitrate samples.
// Persistence stays on the writer thread and does not depend on sample_secs.
const HEARTBEAT_SECS: u64 = 10;

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
    pub secrets: Mutex<Option<Vec<String>>>,
    pub enabled: bool,
}

struct Handle {
    shared: Arc<Shared>,
    flush: mpsc::Sender<oneshot::Sender<bool>>,
}

static HANDLE: OnceLock<Handle> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);
/// Seq of the open relay session (0 = none), read by the sampler.
static OPEN_SEQ: AtomicU64 = AtomicU64::new(0);
static OPEN_ROOM: AtomicU64 = AtomicU64::new(0);
static BILI_SAMPLE_SECS: AtomicU64 = AtomicU64::new(5);

pub fn bili_sample_secs() -> u64 {
    BILI_SAMPLE_SECS.load(Ordering::Relaxed)
}

/// Snapshot the relay identity while observing room counts. The sequence is
/// read twice so a simultaneous room/session switch cannot mix identities.
pub fn open_session_seq(room: u64) -> Option<u64> {
    let before = OPEN_SEQ.load(Ordering::Acquire);
    let observed_room = OPEN_ROOM.load(Ordering::Acquire);
    let after = OPEN_SEQ.load(Ordering::Acquire);
    (before != 0 && before == after && room == observed_room).then_some(before)
}

fn now() -> u64 {
    crate::storage::now()
}

pub(crate) fn shared() -> Option<Arc<Shared>> {
    HANDLE.get().map(|h| Arc::clone(&h.shared))
}

pub fn recording_enabled() -> bool {
    HANDLE.get().is_some_and(|h| h.shared.enabled)
}

/// Never blocks or awaits. A full queue drops and counts.
pub fn emit(kind: EventKind) {
    crate::events::emit(kind);
}

pub fn emit_at(at: u64, kind: EventKind) {
    crate::events::emit_at(at, kind);
}

/// Commit facts already queued before graceful shutdown. This control queue
/// is separate from nonblocking producer dispatch and has a bounded wait.
pub async fn flush(timeout: Duration) -> bool {
    let Some(handle) = HANDLE.get() else {
        return true;
    };
    if !handle.shared.enabled {
        return true;
    }
    request_flush(&handle.flush, timeout).await
}

pub(crate) async fn request_flush(
    sender: &mpsc::Sender<oneshot::Sender<bool>>,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        let (ack, received) = oneshot::channel();
        if sender.send(ack).await.is_err() {
            return false;
        }
        received.await.unwrap_or(false)
    })
    .await
    .unwrap_or(false)
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

/// Register before the process bus is sealed. Database I/O stays on the
/// dedicated writer thread, independently of the WebUI serving future.
pub async fn start() {
    if HANDLE.get().is_some() {
        return;
    }
    let Ok(cfg) = crate::config::load_config().await else {
        return;
    };
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
    capture::set_enabled(cfg.history.capture);
    BILI_SAMPLE_SECS.store(cfg.history.bili_sample_secs.clamp(5, 60), Ordering::Relaxed);
    let shared = Arc::new(Shared {
        path,
        recording_since: now(),
        dropped: AtomicU64::new(0),
        status: Mutex::new(WriterStatus {
            state: "running",
            ..Default::default()
        }),
        cfg: Mutex::new(cfg.history.clone()),
        secrets: Mutex::new(None),
        enabled: cfg.history.enabled,
    });
    let (flush, flush_rx) = mpsc::channel(1);
    if HANDLE
        .set(Handle {
            shared: Arc::clone(&shared),
            flush,
        })
        .is_err()
    {
        return;
    }
    let rx = crate::events::register("history", QUEUE, |_| {
        HANDLE.get().is_some_and(|h| h.shared.enabled)
    });
    if !cfg.history.enabled {
        set_state(&shared, "disabled", None);
        return;
    }
    set_state(&shared, "starting", None);
    let thread_shared = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("history-writer".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let conn = schema::open_rw(&thread_shared.path)?;
                let mut writer = writer::Writer::new(conn, &node, boot_id());
                writer.set_secrets(secrets_of(&cfg));
                live_book::restore(writer::load_live_book(&writer.conn)?);
                run_writer(writer, rx, flush_rx, Arc::clone(&thread_shared), true);
                Ok::<(), rusqlite::Error>(())
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!("统计数据库无法打开，统计已停用: {error}");
                    set_state(&thread_shared, "error", Some(error.to_string()));
                }
                Err(_) => {
                    set_state(
                        &thread_shared,
                        "error",
                        Some("writer thread stopped".into()),
                    );
                    tracing::warn!("统计写入线程已停止");
                }
            }
        });
    if spawned.is_err() {
        set_state(&shared, "error", Some("writer thread".into()));
        return;
    }
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
    BILI_SAMPLE_SECS.store(cfg.history.bili_sample_secs.clamp(5, 60), Ordering::Relaxed);
    if let Some(shared) = shared() {
        *shared.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg.history.clone();
        capture::set_enabled(cfg.history.capture);
        shared
            .secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(secrets_of(cfg));
    }
}

fn flush_queued(w: &mut writer::Writer, rx: &mut Receiver<Arc<Event>>, shared: &Shared) -> bool {
    if let Some(secrets) = shared
        .secrets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        w.set_secrets(secrets);
    }
    // All facts emitted before the control request are either already
    // committed or in this FIFO prefix. A live background producer cannot
    // extend this barrier indefinitely by appending more events.
    let mut remaining = rx.len();
    while remaining > 0 {
        let retention = shared
            .cfg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retention
            .clone();
        match maintain::configure_limits(&w.conn, &retention)
            .and_then(|_| maintain::admit_write(&w.conn, &retention))
        {
            Ok(true) => {}
            result => {
                let error = result.err().map(|error| error.to_string());
                set_state(
                    shared,
                    if error.is_some() { "error" } else { "paused" },
                    error,
                );
                return false;
            }
        }
        let mut batch = Vec::with_capacity(remaining.min(BATCH_ROWS));
        for _ in 0..remaining.min(BATCH_ROWS) {
            match rx.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        remaining -= batch.len();
        if let Err(error) = w.commit_refs(batch.iter().map(AsRef::as_ref)) {
            shared
                .dropped
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            set_state(shared, "error", Some(error.to_string()));
            return false;
        }
        let mut status = shared.status.lock().unwrap_or_else(|e| e.into_inner());
        status.state = "running";
        status.last_batch_at = Some(now());
        status.last_error = None;
    }
    remaining == 0 && shared.dropped.load(Ordering::Relaxed) == 0
}

pub(crate) fn run_writer(
    mut w: writer::Writer,
    mut rx: Receiver<Arc<Event>>,
    mut flush_rx: Receiver<oneshot::Sender<bool>>,
    shared: Arc<Shared>,
    publish: bool,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            set_state(&shared, "error", Some("writer timer unavailable".into()));
            return;
        }
    };
    runtime.block_on(async {
        let mut last_hb = now();
        let mut last_maint = now();
        let mut last_pub = 0u64;
        let mut pending_pub = false;
        loop {
            let mut batch = Vec::new();
            let next = tokio::select! {
                biased;
                Some(ack) = flush_rx.recv() => {
                    let _ = ack.send(flush_queued(&mut w, &mut rx, &shared));
                    continue;
                },
                next = tokio::time::timeout(Duration::from_secs(1), rx.recv()) => next,
            };
            match next {
                Ok(Some(ev)) => {
                    batch.push(ev);
                    while batch.len() < BATCH_ROWS {
                        match rx.try_recv() {
                            Ok(ev) => batch.push(ev),
                            Err(_) => break,
                        }
                    }
                }
                Err(_) => {}
                Ok(None) => break,
            }
            if let Some(secrets) = shared
                .secrets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                w.set_secrets(secrets);
            }
            let t = now();
            let retention = shared
                .cfg
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retention
                .clone();
            // Apply changed limits before every batch. A pinned WAL pauses
            // all history mutations, including maintenance and heartbeats.
            let admitted = maintain::configure_limits(&w.conn, &retention)
                .and_then(|_| maintain::admit_write(&w.conn, &retention));
            match admitted {
                Ok(true) => {}
                result => {
                    shared
                        .dropped
                        .fetch_add(batch.len() as u64, Ordering::Relaxed);
                    let error = result.err().map(|e| e.to_string());
                    set_state(
                        &shared,
                        if error.is_some() { "error" } else { "paused" },
                        error,
                    );
                    continue;
                }
            }
            if !batch.is_empty() {
                match w.commit_refs(batch.iter().map(AsRef::as_ref)) {
                    Ok(_) => {
                        let mut s = shared.status.lock().unwrap_or_else(|e| e.into_inner());
                        s.state = "running";
                        s.last_batch_at = Some(t);
                        s.last_error = None;
                        pending_pub = true;
                    }
                    Err(error) => {
                        shared
                            .dropped
                            .fetch_add(batch.len() as u64, Ordering::Relaxed);
                        set_state(&shared, "error", Some(error.to_string()));
                        // A full file can recover by pruning closed history.
                        last_maint = 0;
                    }
                }
            }
            // SSE `history` hint, at most one per 5 s.
            if publish && pending_pub && t >= last_pub + 5 {
                crate::webui::events::publish(crate::webui::events::HISTORY);
                last_pub = t;
                pending_pub = false;
            }
            if t >= last_hb + HEARTBEAT_SECS {
                let _ = w.heartbeat(t);
                last_hb = t;
            }
            if t >= last_maint + 60 {
                if let Err(error) = maintain::tick(&w.conn, t as i64, &retention) {
                    tracing::debug!("统计维护失败: {error}");
                }
                last_maint = t;
            }
        }
        set_state(&shared, "stopped", None);
    });
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
        Self::start_with_area(cfg, platform, channel_id, channel_name, title, None)
    }

    pub fn start_with_area(
        cfg: &crate::config::Config,
        platform: &str,
        channel_id: &str,
        channel_name: &str,
        title: Option<&str>,
        area_v2: Option<u64>,
    ) -> Self {
        refresh_config(cfg);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let room = cfg.bililive.room.max(0) as u64;
        let live_key = live_book::merged(cfg)
            .open_key(room)
            .filter(|mark| !live_book::needs_revalidation(room, &mark.live_key))
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
            area_v2,
            room,
        });
        OPEN_SEQ.store(0, Ordering::Release);
        OPEN_ROOM.store(room, Ordering::Release);
        OPEN_SEQ.store(seq, Ordering::Release);
        if live_key.is_none() && HANDLE.get().is_some() {
            // Read-only GetInfo fallback, once per session start, off the
            // stream path.
            let cfg = cfg.clone();
            let generation = bili::room_generation(room);
            tokio::spawn(async move {
                if let Some(key) = bili::adopt_room_live_key_for_observation(&cfg, generation).await
                {
                    emit(EventKind::SessionLiveKey { seq, live_key: key });
                }
            });
        }
        Self { seq, done: false }
    }

    pub fn run_started(&self, run: u64) {
        emit(EventKind::FfmpegStarted { seq: self.seq, run });
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Classification may require source rechecks; keep the actual wait
    /// completion time so those checks are recorded as downtime.
    pub fn run_exited_at(
        &self,
        at: u64,
        run: u64,
        code: Option<i32>,
        success: bool,
        counted_failure: bool,
    ) {
        self.run_exited_with_diagnostic_at(at, run, code, success, counted_failure, None);
    }

    pub fn run_exited_with_diagnostic_at(
        &self,
        at: u64,
        run: u64,
        code: Option<i32>,
        success: bool,
        counted_failure: bool,
        diagnostic: Option<&str>,
    ) {
        emit_at(
            at,
            EventKind::FfmpegExited {
                seq: self.seq,
                run,
                code,
                success,
                counted_failure,
                diagnostic: diagnostic.map(|d| crate::logging::safe_text(d, 100)),
            },
        );
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
