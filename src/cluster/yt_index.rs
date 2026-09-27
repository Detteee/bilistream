//! Cluster single-owner YouTube index.
//!
//! The nodes share one YouTube Data API key pool, so only one of them should
//! spend it. The public-status node is the owner: it runs discovery, keeps the
//! adaptive-cadence index (`plugins::youtube_index`), and its heartbeat carries
//! the index version. Peers download the index when that version moves and
//! answer the monitor, the panels and discovery from it without calling
//! Google. With the cluster off, no public node, or a stale or keyless owner,
//! every node answers for itself as it would outside a cluster.

use super::election::last_seen_is_stale;
use super::state::{
    cluster_control_timeout, cluster_state_read, now_secs, recover_read_lock, recover_write_lock,
    CLUSTER_HTTP_CLIENT,
};
use super::types::{ClusterNodeSnapshot, PeerApiResponse, YtIndexPayload};
use crate::config::Config;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Serves `YtIndexPayload` to peers, under `/api`.
pub(crate) const YT_INDEX_ROUTE: &str = "/cluster/yt-index";
/// The owner caps its index at 2,000 answers of roughly 250 bytes each.
const MAX_INDEX_BYTES: usize = 2 * 1024 * 1024;

/// How this node answers YouTube questions.
#[derive(Clone, Debug, Default)]
pub(crate) enum YtIndexRole {
    /// For itself only, as outside a cluster.
    #[default]
    Standalone,
    /// For the cluster: this is the public-status node and has a usable key.
    Owner,
    /// From the owner's index, never calling Google.
    Peer(Arc<YtIndexPayload>),
}

impl YtIndexRole {
    pub(crate) fn is_peer(&self) -> bool {
        matches!(self, Self::Peer(_))
    }
}

static ROLE: RwLock<YtIndexRole> = RwLock::new(YtIndexRole::Standalone);
/// What this node's heartbeat says about its role, set with `ROLE`.
static NODE_STATE: RwLock<Option<super::types::YtIndexNodeState>> = RwLock::new(None);

/// How this node answers YouTube, for its heartbeat.
pub(crate) fn node_state() -> Option<super::types::YtIndexNodeState> {
    recover_read_lock(&NODE_STATE, "YouTube index node state").clone()
}

/// The panel's view of a role. A standalone public-status node says why it
/// cannot serve; any other standalone node lost the index.
fn describe_role(
    cfg: &Config,
    role: &YtIndexRole,
    followed: Option<&str>,
    has_keys: bool,
) -> Option<super::types::YtIndexNodeState> {
    use super::types::YtIndexNodeState::{Follows, Index, Local};
    let public = &cfg.cluster.public_status;
    if !cfg.cluster.enabled || !public.is_enabled() {
        return None;
    }
    Some(match role {
        YtIndexRole::Owner => Index,
        YtIndexRole::Peer(_) => Follows {
            node: followed.unwrap_or_default().to_string(),
        },
        YtIndexRole::Standalone => Local {
            reason: if !public.runs_on(&cfg.cluster.node_id) {
                "index_down"
            } else if has_keys {
                "budget_spent"
            } else {
                "no_key"
            }
            .to_string(),
        },
    })
}

#[cfg(test)]
tokio::task_local! {
    static TEST_ROLE: YtIndexRole;
}

/// The role the yt-index worker last settled on. Plugin hooks read it on every
/// call, so it needs no config and never waits on the network.
pub(crate) fn yt_index_role() -> YtIndexRole {
    #[cfg(test)]
    if let Ok(role) = TEST_ROLE.try_with(YtIndexRole::clone) {
        return role;
    }
    recover_read_lock(&ROLE, "YouTube index role").clone()
}

/// Runs `f` as if this node held `role`, leaving the process-wide role alone.
#[cfg(test)]
pub(crate) async fn with_yt_index_role<F: std::future::Future>(
    role: YtIndexRole,
    f: F,
) -> F::Output {
    TEST_ROLE.scope(role, f).await
}

/// The index for `GET /api/cluster/yt-index`, only while this node is the owner.
pub(crate) fn owner_yt_index() -> Option<Arc<YtIndexPayload>> {
    matches!(yt_index_role(), YtIndexRole::Owner)
        .then(crate::plugins::youtube_index::published)
        .flatten()
}

/// Version for this node's heartbeat while it serves the index.
pub(crate) fn advertised_version() -> Option<String> {
    owner_yt_index().map(|index| index.version.clone())
}

/// What the config and the owner's last heartbeat call for.
#[derive(Debug, PartialEq)]
enum Wanted {
    Standalone,
    Owner,
    /// The owner advertises `version`; this node still needs a copy of it.
    Peer {
        owner: String,
        version: String,
    },
}

fn wanted_role(
    cfg: &Config,
    owner: Option<&ClusterNodeSnapshot>,
    usable_keys: usize,
    now: u64,
) -> Wanted {
    let public = &cfg.cluster.public_status;
    if !cfg.cluster.enabled || !public.is_enabled() {
        return Wanted::Standalone;
    }
    if public.runs_on(&cfg.cluster.node_id) {
        return if usable_keys > 0 {
            Wanted::Owner
        } else {
            Wanted::Standalone
        };
    }
    let Some(owner) = owner.filter(|owner| !owner.is_local) else {
        return Wanted::Standalone;
    };
    let fresh = owner
        .last_seen
        .is_some_and(|seen| !last_seen_is_stale(seen, cfg, now));
    match &owner.yt_index_version {
        Some(version) if fresh => Wanted::Peer {
            owner: owner.node_id.clone(),
            version: version.clone(),
        },
        _ => Wanted::Standalone,
    }
}

struct PeerCopy {
    owner: String,
    index: Arc<YtIndexPayload>,
    /// Last fetch, or last heartbeat that advertised this same version.
    confirmed_at: Instant,
}

/// A copy counts while it was confirmed within `timeout`, so a failing fetch
/// keeps the last index that long and no longer.
fn peer_role(copy: Option<&PeerCopy>, owner: &str, timeout: Duration, now: Instant) -> YtIndexRole {
    match copy {
        Some(copy) if copy.owner == owner && now.duration_since(copy.confirmed_at) <= timeout => {
            YtIndexRole::Peer(copy.index.clone())
        }
        _ => YtIndexRole::Standalone,
    }
}

/// Only the index node subscribes at the WebSub hub, so the hub pushes once
/// and one node spends `videos.list` confirming. A clustered node that fell
/// back to answering for itself stays quiet (RSS and the playlist backstop
/// cover the outage); with the cluster off, a node behaves as on `main`.
fn websub_allowed(cfg: &Config, role: &YtIndexRole) -> bool {
    !cfg.cluster.enabled || matches!(role, YtIndexRole::Owner)
}

/// Channels whose monitor acts on a go-live anywhere in the cluster. The
/// public-status node runs the only store, but is usually idle with every
/// monitor off, so it takes the channels from the nodes' heartbeats: each
/// node's toggles with its own channel targets, plus this node's config.
pub(crate) fn cluster_monitored_channels(cfg: &Config) -> std::collections::HashSet<String> {
    let mut channels = crate::plugins::youtube::monitored_channels(cfg);
    let state = cluster_state_read();
    channels.extend(monitored_from_nodes(
        state
            .nodes
            .values()
            .filter(|node| node.is_local || !node.health.stale),
    ));
    channels
}

fn monitored_from_nodes<'a>(
    nodes: impl IntoIterator<Item = &'a ClusterNodeSnapshot>,
) -> std::collections::HashSet<String> {
    let mut channels = std::collections::HashSet::new();
    for node in nodes {
        let toggles = &node.monitor_toggles;
        let targets = &node.channel_targets;
        if toggles.youtube_enable_monitor && !targets.youtube_channel_id.is_empty() {
            channels.insert(targets.youtube_channel_id.clone());
        }
        if toggles.priority_channel_enabled
            && toggles.priority_channel_auto_restart
            && !targets.priority_youtube_channel_id.is_empty()
        {
            channels.insert(targets.priority_youtube_channel_id.clone());
        }
    }
    channels
}

/// Monitored channels whose video the new copy answers live and the previous
/// copy did not.
fn went_live_channels(
    previous: Option<&YtIndexPayload>,
    next: &YtIndexPayload,
    monitored: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut channels: Vec<String> = next
        .videos
        .iter()
        .filter_map(|(id, video)| {
            let video = video.as_ref()?;
            let channel = &video.snippet.channel_id;
            if !monitored.contains(channel) {
                return None;
            }
            let before = previous
                .and_then(|copy| copy.videos.get(id))
                .and_then(Option::as_ref);
            crate::plugins::youtube_index::went_live(before, video).then(|| channel.clone())
        })
        .collect();
    channels.sort();
    channels.dedup();
    channels
}

async fn fetch_index(cfg: &Config, owner: &str) -> Result<YtIndexPayload, String> {
    let peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == owner)
        .ok_or_else(|| format!("未找到节点 {owner}"))?;
    let url = format!("{}/api{YT_INDEX_ROUTE}", peer.api_url.trim_end_matches('/'));
    let response = CLUSTER_HTTP_CLIENT
        .get(url)
        .timeout(cluster_control_timeout(cfg))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let bytes = crate::plugins::http::response_bytes_limited(response, MAX_INDEX_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    let envelope: PeerApiResponse<YtIndexPayload> =
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !envelope.success {
        return Err(envelope
            .message
            .unwrap_or_else(|| "节点拒绝提供索引".to_string()));
    }
    let mut index = envelope.data.ok_or_else(|| "节点未返回索引".to_string())?;
    // `yt_confirmed` does not travel. The owner only has these rows because
    // YouTube confirmed them, so here too they drop once its answers omit them.
    for row in &mut index.discovered {
        row.yt_confirmed = true;
    }
    Ok(index)
}

/// Whose answers this node used, to log each change once.
#[derive(Clone, Debug, Default, PartialEq)]
enum Source {
    #[default]
    Local,
    ThisNode,
    Node(String),
}

#[derive(Default)]
struct Worker {
    copy: Option<PeerCopy>,
    source: Source,
    fetch_failing: bool,
}

impl Worker {
    async fn cycle(&mut self, cfg: &Config) -> YtIndexRole {
        let wanted = {
            let usable_keys =
                crate::plugins::youtube_data::usable_key_count(&cfg.youtube_api_keys());
            let state = cluster_state_read();
            let owner = state.nodes.get(cfg.cluster.public_status.node_id.trim());
            wanted_role(cfg, owner, usable_keys, now_secs())
        };
        let (owner, version) = match wanted {
            Wanted::Standalone => {
                self.copy = None;
                return YtIndexRole::Standalone;
            }
            Wanted::Owner => {
                self.copy = None;
                return YtIndexRole::Owner;
            }
            Wanted::Peer { owner, version } => (owner, version),
        };
        match self.copy.as_mut() {
            Some(copy) if copy.owner == owner && copy.index.version == version => {
                copy.confirmed_at = Instant::now();
            }
            _ => match fetch_index(cfg, &owner).await {
                Ok(index) => {
                    self.fetch_failing = false;
                    // The active node is usually a peer with no store of its
                    // own, so its monitor learns of a go-live here.
                    let monitored = crate::plugins::youtube::monitored_channels(cfg);
                    let previous = self.copy.as_ref().map(|copy| copy.index.as_ref());
                    for channel in went_live_channels(previous, &index, &monitored) {
                        crate::plugins::youtube::wake_monitor(&channel);
                    }
                    crate::webui::holodex_list::wake();
                    self.copy = Some(PeerCopy {
                        owner: owner.clone(),
                        index: Arc::new(index),
                        confirmed_at: Instant::now(),
                    });
                }
                Err(e) => {
                    if self.fetch_failing {
                        tracing::debug!("读取节点 {} 的 YouTube 索引失败: {}", owner, e);
                    } else {
                        tracing::warn!("读取节点 {} 的 YouTube 索引失败: {}", owner, e);
                    }
                    self.fetch_failing = true;
                }
            },
        }
        let timeout = Duration::from_secs(cfg.cluster.failover_timeout_secs.max(1));
        peer_role(self.copy.as_ref(), &owner, timeout, Instant::now())
    }

    fn note(&mut self, role: &YtIndexRole) {
        let source = match role {
            YtIndexRole::Standalone => Source::Local,
            YtIndexRole::Owner => Source::ThisNode,
            YtIndexRole::Peer(_) => Source::Node(
                self.copy
                    .as_ref()
                    .map(|copy| copy.owner.clone())
                    .unwrap_or_default(),
            ),
        };
        match (&self.source, &source) {
            (before, after) if before == after => {}
            (_, Source::ThisNode) => {
                tracing::info!("YouTube 索引: 本节点负责集群的 YouTube 查询");
            }
            (_, Source::Node(node)) => tracing::info!("YouTube 索引: 使用节点 {} 的索引", node),
            (Source::Node(node), Source::Local) => {
                tracing::warn!("YouTube 索引: 节点 {} 的索引不可用，回退本地查询", node);
            }
            (_, Source::Local) => {
                tracing::info!("YouTube 索引: 本节点不再负责集群的 YouTube 查询");
            }
        }
        self.source = source;
    }
}

/// Heartbeat cadence, so a new owner version is fetched on the cycle that
/// brought it, and at most the owner refresher's 10s tick.
fn cycle_period(cfg: &Config) -> Duration {
    if !cfg.cluster.enabled {
        return Duration::from_secs(15);
    }
    Duration::from_secs(cfg.cluster.heartbeat_interval_secs.clamp(1, 10))
}

/// Settles the role each cycle. On the owner it also refreshes the index; any
/// other node drops the owner store.
pub(crate) async fn run_yt_index() {
    let mut worker = Worker::default();
    loop {
        let cfg = match crate::config::load_config()
            .await
            .map_err(|e| e.to_string())
        {
            Ok(cfg) => cfg,
            Err(error) => {
                tracing::debug!("YouTube index worker skipped config load: {}", error);
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }
        };
        let role = worker.cycle(&cfg).await;
        worker.note(&role);
        let peer = matches!(role, YtIndexRole::Peer(_));
        crate::plugins::youtube_websub::set_enabled(websub_allowed(&cfg, &role));
        *recover_write_lock(&NODE_STATE, "YouTube index node state") = describe_role(
            &cfg,
            &role,
            worker.copy.as_ref().map(|copy| copy.owner.as_str()),
            !cfg.youtube_api_keys().is_empty(),
        );
        *recover_write_lock(&ROLE, "YouTube index role") = role;
        // `main`'s store worker is the one refresher (and publishes on the
        // public-status node); a peer keeps no store of its own.
        if peer {
            crate::plugins::youtube_index::clear();
        }
        tokio::time::sleep(cycle_period(&cfg)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::empty_node;
    use crate::cluster::tests::test_config;
    use crate::config::ClusterPeer;
    use crate::plugins::holodex::{HolodexChannel, HolodexStream};
    use crate::plugins::youtube_data::{YtLiveDetails, YtSnippet, YtVideo};
    use std::collections::{BTreeMap, HashMap};

    fn peer_config(local: &str) -> Config {
        let mut cfg = test_config(local, 0);
        cfg.cluster.peers = vec![ClusterPeer {
            node_id: "owner".to_string(),
            name: "owner".to_string(),
            api_url: "http://owner".to_string(),
            priority: 0,
        }];
        cfg.cluster.public_status.node_id = "owner".to_string();
        cfg
    }

    fn owner(version: Option<&str>, last_seen: u64) -> ClusterNodeSnapshot {
        let mut node = empty_node("owner", "owner", "http://owner", 0, false, last_seen);
        node.last_seen = Some(last_seen);
        node.yt_index_version = version.map(str::to_string);
        node
    }

    #[test]
    fn the_panel_state_says_who_answers_and_why_not() {
        use super::super::types::YtIndexNodeState::{Follows, Index, Local};
        let local = |reason: &str| {
            Some(Local {
                reason: reason.to_string(),
            })
        };
        let mut peer = peer_config("peer");
        assert_eq!(
            describe_role(&peer, &YtIndexRole::Owner, None, true),
            Some(Index)
        );
        assert_eq!(
            describe_role(&peer, &index(Vec::new(), Vec::new()), Some("owner"), false),
            Some(Follows {
                node: "owner".to_string()
            })
        );
        assert_eq!(
            describe_role(&peer, &YtIndexRole::Standalone, None, true),
            local("index_down")
        );

        let mut public = peer_config("owner");
        public.cluster.public_status.node_id = "owner".to_string();
        assert_eq!(
            describe_role(&public, &YtIndexRole::Standalone, None, false),
            local("no_key")
        );
        assert_eq!(
            describe_role(&public, &YtIndexRole::Standalone, None, true),
            local("budget_spent")
        );

        peer.cluster.enabled = false;
        assert_eq!(
            describe_role(&peer, &YtIndexRole::Owner, None, true),
            None,
            "no cluster"
        );
    }

    #[test]
    fn the_node_state_round_trips_and_is_absent_when_unset() {
        use super::super::types::YtIndexNodeState;
        let mut node = owner(None, 1);
        assert!(serde_json::to_value(&node)
            .unwrap()
            .get("yt_index")
            .is_none());
        for state in [
            YtIndexNodeState::Index,
            YtIndexNodeState::Follows {
                node: "ny".to_string(),
            },
            YtIndexNodeState::Local {
                reason: "no_key".to_string(),
            },
        ] {
            node.yt_index = Some(state.clone());
            let value = serde_json::to_value(&node).unwrap();
            let parsed: ClusterNodeSnapshot = serde_json::from_value(value).unwrap();
            assert_eq!(parsed.yt_index, Some(state));
        }
        let value = serde_json::json!({ "state": "follows", "node": "ny" });
        assert_eq!(
            serde_json::from_value::<YtIndexNodeState>(value).unwrap(),
            YtIndexNodeState::Follows {
                node: "ny".to_string()
            }
        );
    }

    #[test]
    fn only_the_index_node_subscribes_to_websub_in_a_cluster() {
        let mut cfg = peer_config("peer");
        assert!(websub_allowed(&cfg, &YtIndexRole::Owner));
        assert!(
            !websub_allowed(&cfg, &YtIndexRole::Standalone),
            "fallback stays quiet"
        );
        assert!(!websub_allowed(&cfg, &index(Vec::new(), Vec::new())));
        cfg.cluster.enabled = false;
        assert!(websub_allowed(&cfg, &YtIndexRole::Standalone), "as on main");
    }

    #[test]
    fn the_store_follows_the_active_nodes_monitors_not_the_idle_ones() {
        let mut active = owner(None, 1);
        active.monitor_toggles.youtube_enable_monitor = true;
        active.monitor_toggles.priority_channel_enabled = true;
        active.monitor_toggles.priority_channel_auto_restart = true;
        active.channel_targets.youtube_channel_id = "UCtarget".to_string();
        active.channel_targets.priority_youtube_channel_id = "UCpriority".to_string();
        let mut idle = owner(None, 1);
        idle.channel_targets = active.channel_targets.clone();
        assert!(monitored_from_nodes([&idle]).is_empty(), "idle: all off");
        assert_eq!(
            monitored_from_nodes([&idle, &active]),
            ["UCtarget", "UCpriority"].map(str::to_string).into()
        );
        active.monitor_toggles.priority_channel_auto_restart = false;
        assert_eq!(
            monitored_from_nodes([&active]),
            ["UCtarget"].map(str::to_string).into()
        );
    }

    #[test]
    fn the_role_follows_the_public_node_its_keys_and_its_heartbeat() {
        let now = 1_000;
        let cfg = peer_config("peer");
        let fresh = owner(Some("v1"), now);
        let peer = Wanted::Peer {
            owner: "owner".to_string(),
            version: "v1".to_string(),
        };

        let mut off = cfg.clone();
        off.cluster.enabled = false;
        assert_eq!(wanted_role(&off, Some(&fresh), 1, now), Wanted::Standalone);
        let mut unset = cfg.clone();
        unset.cluster.public_status.node_id.clear();
        assert_eq!(
            wanted_role(&unset, Some(&fresh), 1, now),
            Wanted::Standalone
        );

        let local = peer_config("owner");
        assert_eq!(wanted_role(&local, None, 1, now), Wanted::Owner);
        assert_eq!(wanted_role(&local, None, 0, now), Wanted::Standalone);

        // A peer's own keys do not matter while the owner is fresh.
        assert_eq!(wanted_role(&cfg, Some(&fresh), 0, now), peer);
        assert_eq!(wanted_role(&cfg, Some(&fresh), 3, now), peer);

        let timeout = cfg.cluster.failover_timeout_secs;
        let last_fresh = owner(Some("v1"), now - timeout);
        assert_eq!(wanted_role(&cfg, Some(&last_fresh), 0, now), peer);
        let stale = owner(Some("v1"), now - timeout - 1);
        assert_eq!(wanted_role(&cfg, Some(&stale), 3, now), Wanted::Standalone);
        let keyless_or_older = owner(None, now);
        assert_eq!(
            wanted_role(&cfg, Some(&keyless_or_older), 3, now),
            Wanted::Standalone
        );
        assert_eq!(wanted_role(&cfg, None, 3, now), Wanted::Standalone);
    }

    #[test]
    fn a_copy_lasts_one_failover_timeout_past_its_last_confirmation() {
        let t0 = Instant::now();
        let timeout = Duration::from_secs(15);
        let copy = PeerCopy {
            owner: "owner".to_string(),
            index: Arc::default(),
            confirmed_at: t0,
        };
        assert!(peer_role(Some(&copy), "owner", timeout, t0 + timeout).is_peer());
        let later = t0 + timeout + Duration::from_secs(1);
        assert!(!peer_role(Some(&copy), "owner", timeout, later).is_peer());
        assert!(
            !peer_role(Some(&copy), "moved", timeout, t0).is_peer(),
            "the public page moved to another node"
        );
    }

    #[test]
    fn snapshots_without_an_index_version_look_as_before() {
        let node = owner(None, 1);
        let value = serde_json::to_value(&node).unwrap();
        assert!(value.get("yt_index_version").is_none());
        let parsed: ClusterNodeSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.yt_index_version, None);

        let value = serde_json::to_value(owner(Some("v1"), 1)).unwrap();
        assert_eq!(value["yt_index_version"], "v1");

        assert!(value.get("websub").is_none(), "no counts, no field");
        assert_eq!(parsed.websub, None);
        let mut subscribed = owner(Some("v1"), 1);
        subscribed.websub = Some(super::super::types::WebSubCounts {
            verified: 3,
            pending: 1,
            failed: 0,
        });
        let value = serde_json::to_value(&subscribed).unwrap();
        assert_eq!(value["websub"]["verified"], 3);
    }

    fn row(id: &str, status: &str) -> HolodexStream {
        HolodexStream {
            id: id.to_string(),
            title: "holodex title".to_string(),
            stream_type: "stream".to_string(),
            topic_id: None,
            published_at: None,
            available_at: None,
            status: status.to_string(),
            start_scheduled: None,
            start_actual: None,
            live_viewers: None,
            channel: HolodexChannel {
                id: "UCyt-index".to_string(),
                ..Default::default()
            },
            link: None,
            thumbnail: None,
            placeholder_type: None,
            yt_confirmed: false,
        }
    }

    fn video(id: &str, end: Option<&str>) -> YtVideo {
        YtVideo {
            id: id.to_string(),
            snippet: YtSnippet {
                title: "youtube title".to_string(),
                ..Default::default()
            },
            live_streaming_details: Some(YtLiveDetails {
                actual_start_time: Some("2026-09-25T10:00:00Z".to_string()),
                actual_end_time: end.map(str::to_string),
                ..Default::default()
            }),
        }
    }

    fn index(videos: Vec<(&str, Option<YtVideo>)>, discovered: Vec<HolodexStream>) -> YtIndexRole {
        YtIndexRole::Peer(Arc::new(YtIndexPayload {
            version: "v1".to_string(),
            videos: videos
                .into_iter()
                .map(|(id, video)| (id.to_string(), video))
                .collect::<BTreeMap<_, _>>(),
            discovered,
        }))
    }

    #[test]
    fn an_adopted_copy_wakes_only_monitored_channels_that_went_live() {
        let on = |id: &str, channel: &str, end: Option<&str>| {
            let mut v = video(id, end);
            v.snippet.channel_id = channel.to_string();
            (id.to_string(), Some(v))
        };
        let payload = |videos: Vec<(String, Option<YtVideo>)>| YtIndexPayload {
            version: "v".to_string(),
            videos: videos.into_iter().collect(),
            discovered: Vec::new(),
        };
        let monitored: std::collections::HashSet<String> =
            ["UCtarget", "UCpriority"].map(str::to_string).into();
        let before = payload(vec![
            on("was_live", "UCtarget", None),
            on("ended_now", "UCpriority", Some("2026-09-25T11:00:00Z")),
        ]);
        let after = payload(vec![
            on("was_live", "UCtarget", None),
            on("ended_now", "UCpriority", None),
            on("other", "UCroster", None),
        ]);
        assert_eq!(
            went_live_channels(Some(&before), &after, &monitored),
            vec!["UCpriority".to_string()]
        );
        assert_eq!(
            went_live_channels(None, &after, &monitored),
            vec!["UCpriority".to_string(), "UCtarget".to_string()],
            "a first copy counts every live answer"
        );
    }

    fn statuses(rows: &[HolodexStream]) -> HashMap<&str, &str> {
        rows.iter()
            .map(|row| (row.id.as_str(), row.status.as_str()))
            .collect()
    }

    #[tokio::test]
    async fn a_peer_answers_from_the_owners_index_without_a_key_or_google() {
        let role = index(
            vec![
                ("went-live", Some(video("went-live", None))),
                ("ended", Some(video("ended", Some("2026-09-25T11:00:00Z")))),
                ("members", None),
            ],
            Vec::new(),
        );
        let rows = vec![
            row("went-live", "upcoming"),
            row("ended", "live"),
            row("members", "live"),
            row("unknown", "upcoming"),
        ];
        let corrected = with_yt_index_role(
            role.clone(),
            crate::plugins::youtube_data::apply_youtube_overlay(rows),
        )
        .await;
        assert_eq!(
            statuses(&corrected),
            HashMap::from([
                ("went-live", "live"),
                ("members", "live"),
                ("unknown", "upcoming"),
            ])
        );

        // With no key the pool cannot pay for a call, so an answer here came
        // from the index.
        let ids = vec!["went-live".to_string(), "unknown".to_string()];
        let videos = with_yt_index_role(
            role,
            crate::plugins::youtube_data::videos_for(&[], None, ids),
        )
        .await
        .unwrap();
        assert_eq!(videos.keys().collect::<Vec<_>>(), vec!["went-live"]);
    }

    #[tokio::test]
    async fn a_peer_shows_a_dropped_stream_until_the_owner_sees_it_end() {
        let mut dropped = row("dropped", "live");
        dropped.yt_confirmed = true;
        let corrected = |answer: Option<YtVideo>| {
            let role = index(vec![("dropped", answer)], vec![dropped.clone()]);
            with_yt_index_role(role, async {
                let rows =
                    crate::plugins::youtube_rss::merge_discovered(vec![row("listed", "upcoming")]);
                crate::plugins::youtube_data::apply_youtube_overlay(rows).await
            })
        };

        let rows = corrected(Some(video("dropped", None))).await;
        assert_eq!(
            statuses(&rows),
            HashMap::from([("listed", "upcoming"), ("dropped", "live")])
        );
        let rows = corrected(Some(video("dropped", Some("2026-09-25T11:00:00Z")))).await;
        assert_eq!(statuses(&rows), HashMap::from([("listed", "upcoming")]));
        let rows = corrected(None).await;
        assert_eq!(statuses(&rows), HashMap::from([("listed", "upcoming")]));
    }

    #[tokio::test]
    async fn a_peer_fetches_the_index_over_the_cluster_route() {
        crate::install_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            &format!("/api{YT_INDEX_ROUTE}"),
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "success": true,
                    "data": {
                        "version": "v7",
                        "videos": {"dropped": null},
                        "discovered": [row("dropped", "live")],
                    },
                    "message": null,
                }))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut cfg = peer_config("peer");
        cfg.cluster.peers[0].api_url = format!("http://{address}/");

        let fetched = fetch_index(&cfg, "owner").await;
        server.abort();
        let fetched = fetched.unwrap();
        assert_eq!(fetched.version, "v7");
        assert_eq!(fetched.videos.get("dropped"), Some(&None));
        assert!(
            fetched.discovered.iter().all(|row| row.yt_confirmed),
            "rows the owner's answers omit must drop on peers too"
        );
    }
}
