//! Serialized public list snapshots, freshness, and rebuild scheduling.
use super::images::prepare_images;
use super::payload::{build_public_streams, PublicStream};
use crate::plugins::banned_keywords::danmaku_banned_keywords;
use axum::body::Bytes;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// The list rebuilds from `main`'s channels list whenever it, the cluster (the
/// danmaku gate) or the areas change, and at least this often, which keeps the
/// channels list leased (10 min) while the page is served.
const KEEP_LEASE: Duration = Duration::from_secs(4 * 60);
/// Events arriving together share one rebuild.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// Holodex counts as fresh for this many of its intervals, so two missed
/// backstop fetches in a row are tolerated.
const HOLODEX_GRACE_INTERVALS: u32 = 3;

struct PublicStreamListSnapshot {
    body: Bytes,
    etag: String,
    /// When Holodex last answered for the list this body came from.
    holodex_ok_at: Option<Instant>,
    holodex_every: Duration,
}

impl PublicStreamListSnapshot {
    /// Served while either source is healthy: Holodex answered within
    /// `HOLODEX_GRACE_INTERVALS` of its cadence, or the YouTube store answers.
    /// With neither, viewers get 503 rather than commands built on stale data.
    fn response_at(&self, now: Instant, youtube_answering: bool) -> Option<(Bytes, String)> {
        let holodex_fresh = self.holodex_ok_at.is_some_and(|at| {
            now.saturating_duration_since(at) < self.holodex_every * HOLODEX_GRACE_INTERVALS
        });
        (holodex_fresh || youtube_answering).then(|| (self.body.clone(), self.etag.clone()))
    }
}

static SNAPSHOT: RwLock<Option<PublicStreamListSnapshot>> = RwLock::new(None);

pub(in crate::webui::public) async fn current_public_streams() -> Option<(Bytes, String)> {
    let youtube_answering = crate::config::load_config()
        .await
        .is_ok_and(|cfg| crate::plugins::youtube_index::answering(&cfg));
    let guard = SNAPSHOT.read().ok()?;
    guard
        .as_ref()?
        .response_at(Instant::now(), youtube_answering)
}

fn store_snapshot(
    streams: &[PublicStream],
    holodex_ok_at: Option<Instant>,
    holodex_every: Duration,
) {
    let Ok(body) = serde_json::to_vec(streams).map(Bytes::from) else {
        return;
    };
    if let Ok(mut guard) = SNAPSHOT.write() {
        replace_streams_snapshot(&mut guard, body, holodex_ok_at, holodex_every);
    }
}

/// An unchanged body keeps its ETag, so viewers' polls settle for a 304.
fn replace_streams_snapshot(
    slot: &mut Option<PublicStreamListSnapshot>,
    body: Bytes,
    holodex_ok_at: Option<Instant>,
    holodex_every: Duration,
) {
    if let Some(previous) = slot.as_mut() {
        if previous.body == body {
            previous.holodex_ok_at = holodex_ok_at;
            previous.holodex_every = holodex_every;
            return;
        }
    }
    *slot = Some(PublicStreamListSnapshot {
        etag: super::super::body_etag(&body),
        body,
        holodex_ok_at,
        holodex_every,
    });
}

/// Re-applies current areas.json keywords and danmaku bans after an edit,
/// without waiting for the next list change.
pub fn remap_after_areas_change() {
    tokio::spawn(async {
        rebuild_public_streams().await;
    });
}

/// One rebuild from the channels list. Returns false when there is no list,
/// leaving the last snapshot to age out.
async fn rebuild_public_streams() -> bool {
    let Ok(cfg) = crate::config::load_config().await else {
        return false;
    };
    let Ok(channels_data) = crate::config::load_channels() else {
        return false;
    };
    let channels = channels_data.channels;

    // Already merged with discovery, overlaid by YouTube and filtered, the same
    // rows the dashboard shows. Reading it also extends the list's lease.
    let list = match crate::webui::stream_list::current(
        crate::webui::stream_list::ListKind::Channels,
        false,
    )
    .await
    {
        Ok(list) => list,
        Err(e) => {
            tracing::debug!("公开状态页读取直播列表失败: {}", e);
            return false;
        }
    };

    // Config stays on during a restream; the processor gate does not. Use the
    // same owner-facing flag the status cards do, or 切换 stays green while
    // `%转播%` is rejected.
    let cluster = crate::cluster::get_cluster_status_for_config(&cfg).await;
    let danmaku_enabled = super::super::snapshot::public_danmaku_enabled(&cluster);

    let banned = danmaku_banned_keywords();
    let mut public = build_public_streams(list.rows.clone(), &channels, danmaku_enabled, &banned);

    prepare_images(&mut public, &cfg).await;

    store_snapshot(&public, list.holodex_ok_at, list.holodex_every);
    true
}

/// Whether a server event can change the public list: the channels list
/// changed, the cluster (the danmaku gate) changed, or events were missed.
fn rebuild_on(
    event: Result<&'static str, tokio::sync::broadcast::error::RecvError>,
) -> Option<bool> {
    use tokio::sync::broadcast::error::RecvError;
    match event {
        Ok(kind) => {
            Some(kind == crate::webui::events::HOLODEX || kind == crate::webui::events::CLUSTER)
        }
        Err(RecvError::Lagged(_)) => Some(true),
        Err(RecvError::Closed) => None,
    }
}

/// Rebuilds on list and cluster events, and every `KEEP_LEASE`, for as long as
/// this node serves the page. Stops when the page moves elsewhere.
pub(in crate::webui::public) fn start_streams_watch() -> tokio::sync::oneshot::Sender<()> {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut events = crate::AppState::current().subscribe_events();
        loop {
            rebuild_public_streams().await;
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    _ = tokio::time::sleep(KEEP_LEASE) => break,
                    event = events.recv() => match rebuild_on(event) {
                        Some(true) => {
                            tokio::time::sleep(DEBOUNCE).await;
                            // Events in the window share this rebuild.
                            while events.try_recv().is_ok() {}
                            break;
                        }
                        Some(false) => {}
                        None => return,
                    },
                }
            }
        }
    });

    stop_tx
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_list_is_served_while_either_source_is_healthy() {
        let now = Instant::now();
        let every = Duration::from_secs(300);
        let mut slot = None;
        let body = Bytes::from_static(b"[]");
        replace_streams_snapshot(&mut slot, body.clone(), Some(now), every);
        let snapshot = slot.as_ref().unwrap();
        let stale = now + every * HOLODEX_GRACE_INTERVALS;
        assert!(
            snapshot
                .response_at(stale - Duration::from_secs(1), false)
                .is_some(),
            "Holodex only"
        );
        assert!(snapshot.response_at(stale, true).is_some(), "YouTube only");
        assert!(snapshot.response_at(stale, false).is_none(), "neither: 503");

        let mut never = None;
        replace_streams_snapshot(&mut never, body.clone(), None, every);
        assert!(never.as_ref().unwrap().response_at(now, false).is_none());
        assert!(never.as_ref().unwrap().response_at(now, true).is_some());
    }

    #[test]
    fn an_unchanged_body_keeps_its_etag_and_takes_the_new_holodex_time() {
        let now = Instant::now();
        let every = Duration::from_secs(60);
        let mut slot = None;
        replace_streams_snapshot(&mut slot, Bytes::from_static(b"[]"), Some(now), every);
        let etag = slot.as_ref().unwrap().etag.clone();
        let later = now + Duration::from_secs(120);
        replace_streams_snapshot(&mut slot, Bytes::from_static(b"[]"), Some(later), every);
        assert_eq!(slot.as_ref().unwrap().etag, etag);
        assert_eq!(slot.as_ref().unwrap().holodex_ok_at, Some(later));
        replace_streams_snapshot(&mut slot, Bytes::from_static(b"[1]"), Some(later), every);
        assert_ne!(slot.as_ref().unwrap().etag, etag);
    }

    #[test]
    fn only_list_and_cluster_events_rebuild() {
        use tokio::sync::broadcast::error::RecvError;
        assert_eq!(rebuild_on(Ok(crate::webui::events::HOLODEX)), Some(true));
        assert_eq!(rebuild_on(Ok(crate::webui::events::CLUSTER)), Some(true));
        assert_eq!(rebuild_on(Ok(crate::webui::events::STATUS)), Some(false));
        assert_eq!(rebuild_on(Err(RecvError::Lagged(3))), Some(true));
        assert_eq!(rebuild_on(Err(RecvError::Closed)), None);
    }
}
