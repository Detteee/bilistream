//! Ordinary cluster settings shared on signed routes. Topology, identities and
//! the public/index role assignment never travel here; they change only
//! through membership operations.

use super::membership::Lifecycle;
use super::state::cluster_control_timeout;
use super::sync::summarize_peer_push_results;
use super::types::PeerApiResponse;
use crate::config::{ClusterConfig, ClusterHealthThresholds, Config, PublicStatusConfig};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSettings {
    pub sync_monitored_channels: bool,
    pub auto_failover: bool,
    pub heartbeat_interval_secs: u64,
    pub failover_timeout_secs: u64,
    pub lease_ttl_secs: u64,
    pub thresholds: ClusterHealthThresholds,
    pub public_status: PublicStatusPresentation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicStatusPresentation {
    pub bind: String,
    pub port: u16,
    pub holodex_refresh_secs: u64,
    pub public_url: String,
}

impl ClusterSettings {
    pub fn from_config(cluster: &ClusterConfig) -> Self {
        let public = &cluster.public_status;
        Self {
            sync_monitored_channels: cluster.sync_monitored_channels,
            auto_failover: cluster.auto_failover,
            heartbeat_interval_secs: cluster.heartbeat_interval_secs,
            failover_timeout_secs: cluster.failover_timeout_secs,
            lease_ttl_secs: cluster.lease_ttl_secs,
            thresholds: cluster.thresholds.clone(),
            public_status: PublicStatusPresentation {
                bind: public.bind.clone(),
                port: public.port,
                holodex_refresh_secs: public.holodex_refresh_secs,
                public_url: public.public_url.clone(),
            },
        }
    }

    /// The receiving node keeps its committed public-node assignment.
    pub fn apply(self, cluster: &mut ClusterConfig) -> Result<(), String> {
        let public = PublicStatusConfig {
            node_id: cluster.public_status.node_id.clone(),
            bind: self.public_status.bind,
            port: self.public_status.port,
            holodex_refresh_secs: self.public_status.holodex_refresh_secs,
            public_url: self.public_status.public_url,
        };
        public.validate()?;
        cluster.public_status = public;
        cluster.sync_monitored_channels = self.sync_monitored_channels;
        cluster.auto_failover = self.auto_failover;
        cluster.heartbeat_interval_secs = self.heartbeat_interval_secs.max(1);
        cluster.failover_timeout_secs = self.failover_timeout_secs.max(1);
        cluster.lease_ttl_secs = self.lease_ttl_secs.max(1);
        cluster.thresholds = self.thresholds;
        Ok(())
    }
}

pub async fn push_cluster_settings_to_peers(cfg: &Config) -> Result<usize, String> {
    if !cfg.cluster.enabled {
        return Ok(0);
    }
    let settings = ClusterSettings::from_config(&cfg.cluster);
    let timeout = cluster_control_timeout(cfg);
    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| {
            let settings = &settings;
            async move {
                let reply: PeerApiResponse<()> = super::peer_call::call(
                    cfg,
                    &peer.node_id,
                    super::peer_call::routes::SETTINGS,
                    Some(settings),
                    timeout,
                )
                .await
                .map_err(|e| format!("{} {}", peer.node_id, e))?;
                if reply.success {
                    Ok(())
                } else {
                    Err(format!(
                        "{} {}",
                        peer.node_id,
                        reply.message.unwrap_or_else(|| "同步被拒绝".into())
                    ))
                }
            }
        });
    summarize_peer_push_results(join_all(tasks).await, "部分节点集群设置同步失败")
}

pub(crate) const TOPOLOGY_EDIT_REJECTED: &str =
    "集群成员、节点身份和状态页节点只能通过成员操作更改，请使用多服务器设置中的成员操作";

/// Generic form saves may carry unchanged topology for compatibility. Any
/// change to it is refused; only ordinary settings are taken from the form.
pub(crate) fn fence_cluster_edit(
    current: &ClusterConfig,
    requested: ClusterConfig,
    lifecycle: Lifecycle,
) -> Result<ClusterConfig, &'static str> {
    let same_peers =
        serde_json::to_value(&current.peers).ok() == serde_json::to_value(&requested.peers).ok();
    let same_identity = current.node_id == requested.node_id
        && current.node_name == requested.node_name
        && current.public_api_url == requested.public_api_url
        && current.priority == requested.priority
        && current.public_status.node_id == requested.public_status.node_id;
    // Standalone nodes keep their local label for the public page; they can
    // never enable clustering or edit a peer list here.
    let standalone_local_edit =
        lifecycle == Lifecycle::Standalone && !current.enabled && !requested.enabled;
    if current.enabled != requested.enabled
        || !same_peers
        || (!same_identity && !standalone_local_edit)
    {
        return Err(TOPOLOGY_EDIT_REJECTED);
    }
    Ok(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_edits_keep_topology_and_settings_payload_has_no_role_or_members() {
        let mut current = super::super::tests::test_config("ca", 7).cluster;
        current.peers = vec![crate::config::ClusterPeer {
            node_id: "jp".into(),
            name: "JP".into(),
            api_url: "https://jp.example".into(),
            priority: 10,
        }];
        current.public_status.node_id = "ca".into();
        let mut ordinary = current.clone();
        ordinary.auto_failover = !current.auto_failover;
        assert!(fence_cluster_edit(&current, ordinary, Lifecycle::Managed).is_ok());
        for edit in [
            |c: &mut ClusterConfig| c.peers.clear(),
            |c: &mut ClusterConfig| c.enabled = !c.enabled,
            |c: &mut ClusterConfig| c.node_id = "other".into(),
            |c: &mut ClusterConfig| c.priority += 1,
            |c: &mut ClusterConfig| c.public_status.node_id = "jp".into(),
        ] {
            let mut changed = current.clone();
            edit(&mut changed);
            assert_eq!(
                fence_cluster_edit(&current, changed, Lifecycle::Managed).unwrap_err(),
                TOPOLOGY_EDIT_REJECTED
            );
        }
        let mut standalone = current.clone();
        standalone.enabled = false;
        standalone.peers.clear();
        let mut renamed = standalone.clone();
        renamed.node_name = "Renamed".into();
        assert!(fence_cluster_edit(&standalone, renamed.clone(), Lifecycle::Standalone).is_ok());
        assert!(fence_cluster_edit(&standalone, renamed, Lifecycle::Left).is_err());
        let mut enable = standalone.clone();
        enable.enabled = true;
        assert!(fence_cluster_edit(&standalone, enable, Lifecycle::Standalone).is_err());

        let value = serde_json::to_value(ClusterSettings::from_config(&current)).unwrap();
        let text = value.to_string();
        for forbidden in ["peers", "node_id", "public_api_url", "priority", "\"jp\""] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
        let mut injected = value.clone();
        injected["public_status"]["node_id"] = "jp".into();
        assert!(serde_json::from_value::<ClusterSettings>(injected).is_err());
        let mut receiver = current.clone();
        let mut settings = ClusterSettings::from_config(&current);
        settings.public_status.port = 24000;
        settings.apply(&mut receiver).unwrap();
        assert_eq!(receiver.public_status.node_id, "ca");
        assert_eq!(receiver.public_status.port, 24000);
    }
}
