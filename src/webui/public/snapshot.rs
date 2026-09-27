//! Short-TTL snapshot of the public payload.
//!
//! Every viewer reads the same rendered bytes: the page is open to anyone, so
//! the cost of one visitor and of ten thousand has to be the same. The payload
//! is rebuilt at most once per [`SNAPSHOT_TTL`], and an ETag lets repeat polls
//! settle for a 304.

use axum::body::Bytes;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use super::payload::PublicStatus;
use crate::cluster::{ClusterNodeSnapshot, ClusterStatus};
use crate::plugins::is_danmaku_commands_enabled;
use crate::webui::state::{get_status_cache, StatusData};

/// Longer than this and the page lags the dashboard noticeably; shorter and a
/// busy page rebuilds the payload for no visible gain.
pub const SNAPSHOT_TTL: Duration = Duration::from_secs(5);

struct Snapshot {
    body: Bytes,
    etag: String,
    built_at: Instant,
}

static SNAPSHOT: RwLock<Option<Snapshot>> = RwLock::new(None);
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The status the page should show, or `None` when this node has no usable
/// view of whoever owns the stream — which is what `in_sync` reports.
///
/// On a standby the streaming node's state arrives through cluster heartbeats,
/// so the owner's snapshot is the source. A snapshot that has gone stale is
/// withheld rather than served as current.
fn public_status_source(cluster: &ClusterStatus) -> Option<StatusData> {
    if !cluster.enabled {
        // Single node: it is the one streaming, so its own cache is the truth.
        return get_status_cache().map(with_live_danmaku_gate);
    }

    let owner = cluster.active_owner.as_deref()?;
    let node = cluster.nodes.iter().find(|node| node.node_id == owner)?;

    if node.is_local {
        return get_status_cache().map(with_live_danmaku_gate);
    }
    node_status_if_fresh(node)
}

/// Cache still holds the config switch. Restreaming clears the processor that
/// accepts `%转播%`, and that is what the page must advertise.
fn with_live_danmaku_gate(mut status: StatusData) -> StatusData {
    status.bilibili.enable_danmaku_command = is_danmaku_commands_enabled();
    status
}

/// Whether viewers can send a 切换 command right now: the owner's processor
/// gate, not the config switch. A restreaming node keeps the config on and
/// disables commands until ffmpeg exits.
///
/// The 转播中 badge is ffmpeg+RTMP from this same cluster view. If that is
/// already on and the processor flag is still the config bit, advertise off
/// here rather than polling faster or shortening the 5s snapshot.
pub(super) fn public_danmaku_enabled(cluster: &ClusterStatus) -> bool {
    if cluster.owner_is_restreaming() {
        return false;
    }
    public_status_source(cluster)
        .map(|status| status.bilibili.enable_danmaku_command)
        .unwrap_or(false)
}

fn node_status_if_fresh(node: &ClusterNodeSnapshot) -> Option<StatusData> {
    if node.health.stale {
        return None;
    }
    node.status.clone()
}

fn build_public_status(cluster: &ClusterStatus) -> PublicStatus {
    let mut payload = PublicStatus::build(public_status_source(cluster).as_ref(), cluster);
    if cluster.owner_is_restreaming() {
        payload.bilibili.enable_danmaku_command = false;
    }
    payload
}

/// Rebuilds the cached payload unconditionally.
async fn refresh_public_status() {
    let Ok(cfg) = crate::config::load_config().await else {
        return;
    };
    let cluster = crate::cluster::get_cluster_status_for_config(&cfg).await;
    let payload = build_public_status(&cluster);
    store_snapshot(payload);
}

fn store_snapshot(payload: PublicStatus) {
    let Ok(body) = serde_json::to_vec(&payload).map(Bytes::from) else {
        return;
    };
    let etag = super::body_etag(&body);

    if let Ok(mut guard) = SNAPSHOT.write() {
        // Keep the previous ETag when nothing changed, so a polling client
        // stays on 304s instead of re-downloading identical bytes.
        if let Some(previous) = guard.as_mut() {
            if previous.body == body {
                previous.built_at = Instant::now();
                return;
            }
        }
        *guard = Some(Snapshot {
            body,
            etag,
            built_at: Instant::now(),
        });
    }
}

/// The cached body and its ETag, rebuilding first when the cache has expired.
pub(super) async fn current_public_status() -> Option<(Bytes, String)> {
    if snapshot_is_fresh() {
        return read_snapshot();
    }
    let _refresh = REFRESH_LOCK.lock().await;
    if !snapshot_is_fresh() {
        refresh_public_status().await;
    }
    read_snapshot()
}

/// Drop the TTL so the next poll rebuilds. Used when the owner's processor
/// gate flips: 转播 ending enables `%转播%` and the page should not wait 5s
/// to notice. Retain bytes for ETag reuse, but never serve an expired snapshot
/// as current if rebuilding fails.
pub(crate) fn invalidate_public_status_snapshot() {
    if let Ok(mut guard) = SNAPSHOT.write() {
        if let Some(snap) = guard.as_mut() {
            snap.built_at = Instant::now()
                .checked_sub(SNAPSHOT_TTL)
                .unwrap_or(snap.built_at);
        }
    }
}

fn snapshot_is_fresh() -> bool {
    SNAPSHOT
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .map(|snap| snap.built_at.elapsed() < SNAPSHOT_TTL)
        })
        .unwrap_or(false)
}

fn read_snapshot() -> Option<(Bytes, String)> {
    let guard = SNAPSHOT.read().ok()?;
    let snapshot = guard.as_ref()?;
    if snapshot.built_at.elapsed() >= SNAPSHOT_TTL {
        return None;
    }
    Some((snapshot.body.clone(), snapshot.etag.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{ClusterHealth, ClusterNodeRole};
    use crate::webui::state::BiliStatus;

    fn node(node_id: &str, is_local: bool, stale: bool, title: &str) -> ClusterNodeSnapshot {
        ClusterNodeSnapshot {
            self_check: None,
            node_id: node_id.to_string(),
            name: node_id.to_string(),
            api_url: String::new(),
            priority: 0,
            last_seen: Some(1),
            is_local,
            role: ClusterNodeRole::Active,
            health: if stale {
                ClusterHealth::unhealthy("stale_heartbeat", true, false)
            } else {
                ClusterHealth::healthy()
            },
            draining: false,
            network_unstable: false,
            ffmpeg_running: true,
            active_stream: None,
            status: Some(StatusData {
                bilibili: BiliStatus {
                    is_live: true,
                    title: title.to_string(),
                    ..BiliStatus::default()
                },
                youtube: None,
                twitch: None,
                niconico: None,
                priority_channel: None,
            }),
            network: None,
            config_version: String::new(),
            failed_restarts: 0,
            monitor_toggles: Default::default(),
            channel_targets: Default::default(),
            yt_index_version: None,
            websub: None,
            yt_index: None,
        }
    }

    fn cluster(active_owner: Option<&str>, nodes: Vec<ClusterNodeSnapshot>) -> ClusterStatus {
        ClusterStatus {
            enabled: true,
            local_node_id: "ny".to_string(),
            active_owner: active_owner.map(|owner| owner.to_string()),
            lease_until: None,
            config_version: String::new(),
            auto_failover: true,
            public_status: Default::default(),
            nodes,
        }
    }

    #[test]
    fn standby_serves_the_owners_snapshot() {
        let cluster = cluster(Some("jp"), vec![node("jp", false, false, "live in jp")]);
        let payload = build_public_status(&cluster);

        assert!(payload.in_sync);
        assert_eq!(payload.bilibili.title, "live in jp");
    }

    #[test]
    fn a_stale_owner_is_reported_out_of_sync_not_served_stale() {
        let cluster = cluster(Some("jp"), vec![node("jp", false, true, "hours old")]);
        let payload = build_public_status(&cluster);

        assert!(!payload.in_sync);
        assert!(payload.bilibili.title.is_empty());
    }

    #[test]
    fn no_active_owner_is_out_of_sync() {
        let cluster = cluster(None, vec![node("jp", false, false, "live in jp")]);
        assert!(!build_public_status(&cluster).in_sync);
    }

    #[test]
    fn an_owner_missing_from_the_node_list_is_out_of_sync() {
        let cluster = cluster(Some("ca"), vec![node("jp", false, false, "live in jp")]);
        assert!(!build_public_status(&cluster).in_sync);
    }

    #[test]
    fn out_of_sync_payload_still_lists_the_nodes() {
        let cluster = cluster(Some("jp"), vec![node("jp", false, true, "hours old")]);
        let payload = PublicStatus::build(None, &cluster);

        assert!(!payload.in_sync);
        assert_eq!(payload.nodes.len(), 1);
        assert!(payload.bilibili.title.is_empty());
    }

    #[test]
    fn public_danmaku_follows_the_owners_processor_gate() {
        let mut live = node("jp", false, false, "live in jp");
        live.status
            .as_mut()
            .expect("status")
            .bilibili
            .enable_danmaku_command = true;
        assert!(public_danmaku_enabled(&cluster(
            Some("jp"),
            vec![live.clone()]
        )));

        live.status
            .as_mut()
            .expect("status")
            .bilibili
            .enable_danmaku_command = false;
        assert!(!public_danmaku_enabled(&cluster(Some("jp"), vec![live])));
    }
}
