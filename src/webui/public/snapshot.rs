//! Short-TTL snapshot of the public payload.
//!
//! Every viewer reads the same rendered bytes: the page is open to anyone, so
//! the cost of one visitor and of ten thousand has to be the same. The payload
//! is rebuilt at most once per [`SNAPSHOT_TTL`], and an ETag lets repeat polls
//! settle for a 304.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use super::payload::PublicStatus;
use crate::cluster::{ClusterNodeSnapshot, ClusterStatus};
use crate::webui::state::{get_status_cache, StatusData};

/// Longer than this and the page lags the dashboard noticeably; shorter and a
/// busy page rebuilds the payload for no visible gain.
pub const SNAPSHOT_TTL: Duration = Duration::from_secs(5);

struct Snapshot {
    body: String,
    etag: String,
    built_at: Instant,
}

static SNAPSHOT: RwLock<Option<Snapshot>> = RwLock::new(None);
static ETAG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The status the page should show, and whether it is a live view.
///
/// On a standby the streaming node's state arrives through cluster heartbeats,
/// so the owner's snapshot is the source. A snapshot that has gone stale is
/// reported as out of sync rather than served as current.
pub fn public_status_source(cluster: &ClusterStatus) -> (Option<StatusData>, bool) {
    if !cluster.enabled {
        // Single node: it is the one streaming, so its own cache is the truth.
        return (get_status_cache(), true);
    }

    let Some(owner) = cluster.active_owner.as_deref() else {
        return (None, false);
    };

    let Some(node) = cluster.nodes.iter().find(|node| node.node_id == owner) else {
        return (None, false);
    };

    if node.is_local {
        return (get_status_cache(), true);
    }

    match node_status_if_fresh(node) {
        Some(status) => (Some(status), true),
        None => (None, false),
    }
}

fn node_status_if_fresh(node: &ClusterNodeSnapshot) -> Option<StatusData> {
    if node.health.stale {
        return None;
    }
    node.status.clone()
}

pub fn build_public_status(cluster: &ClusterStatus) -> PublicStatus {
    let (status, in_sync) = public_status_source(cluster);
    let mut payload = PublicStatus::build(status.as_ref(), cluster);
    payload.in_sync = in_sync && payload.in_sync;
    payload
}

/// Rebuilds the cached payload unconditionally.
pub async fn refresh_public_status() {
    let Ok(cfg) = crate::config::load_config().await else {
        return;
    };
    let cluster = crate::cluster::get_cluster_status_for_config(&cfg).await;
    let payload = build_public_status(&cluster);
    store_snapshot(payload);
}

fn store_snapshot(payload: PublicStatus) {
    let Ok(body) = serde_json::to_string(&payload) else {
        return;
    };
    let etag = format!("\"{:x}\"", ETAG_COUNTER.fetch_add(1, Ordering::Relaxed));

    if let Ok(mut guard) = SNAPSHOT.write() {
        // Keep the previous ETag when nothing changed, so a polling client
        // stays on 304s instead of re-downloading identical bytes.
        if let Some(previous) = guard.as_ref() {
            if previous.body == body {
                *guard = Some(Snapshot {
                    body: previous.body.clone(),
                    etag: previous.etag.clone(),
                    built_at: Instant::now(),
                });
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
pub async fn current_public_status() -> Option<(String, String)> {
    if snapshot_is_fresh() {
        return read_snapshot();
    }
    refresh_public_status().await;
    read_snapshot()
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

fn read_snapshot() -> Option<(String, String)> {
    let guard = SNAPSHOT.read().ok()?;
    let snapshot = guard.as_ref()?;
    Some((snapshot.body.clone(), snapshot.etag.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{ClusterHealth, ClusterNodeRole};
    use crate::webui::state::BiliStatus;

    fn node(node_id: &str, is_local: bool, stale: bool, title: &str) -> ClusterNodeSnapshot {
        ClusterNodeSnapshot {
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
        let (status, in_sync) = public_status_source(&cluster);

        assert!(in_sync);
        assert_eq!(status.unwrap().bilibili.title, "live in jp");
    }

    #[test]
    fn a_stale_owner_is_reported_out_of_sync_not_served_stale() {
        let cluster = cluster(Some("jp"), vec![node("jp", false, true, "hours old")]);
        let (status, in_sync) = public_status_source(&cluster);

        assert!(!in_sync);
        assert!(status.is_none());
    }

    #[test]
    fn no_active_owner_is_out_of_sync() {
        let cluster = cluster(None, vec![node("jp", false, false, "live in jp")]);
        let (status, in_sync) = public_status_source(&cluster);

        assert!(!in_sync);
        assert!(status.is_none());
    }

    #[test]
    fn an_owner_missing_from_the_node_list_is_out_of_sync() {
        let cluster = cluster(Some("ca"), vec![node("jp", false, false, "live in jp")]);
        let (status, in_sync) = public_status_source(&cluster);

        assert!(!in_sync);
        assert!(status.is_none());
    }

    #[test]
    fn out_of_sync_payload_still_lists_the_nodes() {
        let cluster = cluster(Some("jp"), vec![node("jp", false, true, "hours old")]);
        let payload = PublicStatus::build(None, &cluster);

        assert!(!payload.in_sync);
        assert_eq!(payload.nodes.len(), 1);
        assert!(payload.bilibili.title.is_empty());
    }
}
