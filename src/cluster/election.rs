//! Owner election: eligibility, last-resort, and peer owner-view adoption.

use super::state::{cluster_state_read, now_secs, ClusterState};
use super::types::*;
use crate::config::Config;
use std::collections::HashSet;

pub(crate) fn replacement_owner_for_drain(cfg: &Config, source_node_id: &str) -> Option<String> {
    if !cfg.cluster.auto_failover {
        return None;
    }

    let mut state = cluster_state_read().clone();
    state.forced_owner = None;
    if source_node_id == cfg.cluster.node_id {
        state.local_draining = true;
    }
    if let Some(source) = state.nodes.get_mut(source_node_id) {
        source.draining = true;
    }

    let configured = configured_node_ids(cfg);
    choose_owner_with_configured(&state, cfg, now_secs(), &configured)
        .filter(|owner| owner != source_node_id)
}

pub(crate) fn clear_invalid_forced_owner(
    state: &mut ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) {
    let forced_owner_is_eligible = state
        .forced_owner
        .as_ref()
        .and_then(|owner| state.nodes.get(owner))
        .is_some_and(|node| node_is_eligible(node, state, cfg, now, configured));

    if state.forced_owner.is_some() && !forced_owner_is_eligible {
        state.forced_owner = None;
    }
}

#[cfg(test)]
pub(crate) fn choose_owner(state: &ClusterState, cfg: &Config, now: u64) -> Option<String> {
    let configured = configured_node_ids(cfg);
    choose_owner_with_configured(state, cfg, now, &configured)
}

pub(crate) fn choose_owner_with_configured(
    state: &ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) -> Option<String> {
    let forced_owner = state
        .forced_owner
        .as_ref()
        .filter(|owner| {
            state
                .nodes
                .get(*owner)
                .is_some_and(|node| node_is_eligible(node, state, cfg, now, configured))
        })
        .cloned();
    if forced_owner.is_some() {
        return forced_owner;
    }

    if !cfg.cluster.auto_failover {
        return state
            .active_owner
            .as_ref()
            .filter(|owner| configured.contains(owner.as_str()))
            .cloned()
            .or_else(|| last_resort_local_owner(state, cfg));
    }

    if let Some(current_owner) = state.active_owner.as_ref() {
        if state
            .nodes
            .get(current_owner)
            .is_some_and(|node| node_is_eligible(node, state, cfg, now, configured))
        {
            return Some(current_owner.clone());
        }
    }

    state
        .nodes
        .values()
        .filter(|node| node_is_eligible(node, state, cfg, now, configured))
        .max_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| b.node_id.cmp(&a.node_id))
        })
        .map(|node| node.node_id.clone())
        .or_else(|| last_resort_local_owner(state, cfg))
}

/// When every peer is disabled/unavailable, keep an active local node running,
/// but never let an all-off standby self-elect.
pub(crate) fn last_resort_local_owner(state: &ClusterState, cfg: &Config) -> Option<String> {
    if state.active_owner.as_deref() != Some(cfg.cluster.node_id.as_str()) {
        return None;
    }
    if !monitor_toggles_any_enabled(&monitor_toggle_state_from_config(cfg)) {
        return None;
    }

    state
        .nodes
        .get(&cfg.cluster.node_id)
        .filter(|node| !node.draining)
        .map(|_| cfg.cluster.node_id.clone())
}

pub(crate) fn node_is_eligible(
    node: &ClusterNodeSnapshot,
    state: &ClusterState,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) -> bool {
    if node.is_serviceable() && !is_stale(node, cfg, now) {
        return true;
    }
    if node.draining || node.network_unstable || node.node_id == cfg.cluster.node_id {
        return false;
    }
    if !node.health.stale
        && node.health.reason != "api_unreachable"
        && node.health.reason != "heartbeat_timeout"
        && node.health.reason != "waiting_for_heartbeat"
    {
        return false;
    }
    indirectly_observed_by_quorum(state, &node.node_id, cfg, now, configured)
}

pub(crate) fn indirectly_observed_by_quorum(
    state: &ClusterState,
    node_id: &str,
    cfg: &Config,
    now: u64,
    configured: &HashSet<&str>,
) -> bool {
    let Some(observations) = state.peer_observations.get(node_id) else {
        return false;
    };
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    let fresh_observers = observations
        .iter()
        .filter(|(observer, observed_at)| {
            configured.contains(observer.as_str())
                && now.saturating_sub(**observed_at) <= timeout
                && observer_node_is_reliable(state, observer, cfg, now)
        })
        .count();

    fresh_observers >= indirect_observer_threshold(cfg)
}

pub(crate) fn observer_node_is_reliable(
    state: &ClusterState,
    observer_node_id: &str,
    cfg: &Config,
    now: u64,
) -> bool {
    let Some(observer) = state.nodes.get(observer_node_id) else {
        return false;
    };
    observer.is_serviceable() && !is_stale(observer, cfg, now)
}

pub(crate) fn indirect_observer_threshold(cfg: &Config) -> usize {
    let cluster_size = cfg.cluster.peers.len() + 1;
    (cluster_size / 2).max(1)
}

pub(crate) fn configured_node_ids(cfg: &Config) -> HashSet<&str> {
    let mut configured = HashSet::with_capacity(cfg.cluster.peers.len() + 1);
    configured.insert(cfg.cluster.node_id.as_str());
    for peer in &cfg.cluster.peers {
        configured.insert(peer.node_id.as_str());
    }
    configured
}

pub(crate) fn is_stale(node: &ClusterNodeSnapshot, cfg: &Config, now: u64) -> bool {
    node.last_seen
        .map(|last_seen| last_seen_is_stale(last_seen, cfg, now))
        .unwrap_or(false)
}

pub(crate) fn last_seen_is_stale(last_seen: u64, cfg: &Config, now: u64) -> bool {
    now.saturating_sub(last_seen) > cfg.cluster.failover_timeout_secs.max(1)
}

/// Adopts a peer's active-owner view only when it does not regress ours.
/// Without this, a peer holding a stale view could flip `active_owner`
/// back and forth on every heartbeat and defeat owner stickiness.
pub(crate) fn adopt_owner_view(
    state: &mut ClusterState,
    incoming_owner: Option<String>,
    incoming_lease: u64,
    auto_failover: bool,
    direct_peer_id: Option<&str>,
) {
    let Some(incoming_owner) = incoming_owner else {
        // Peer has no owner opinion: keep ours.
        return;
    };
    let direct_owner_claim = direct_peer_id == Some(incoming_owner.as_str());
    let directly_reported_owner_is_eligible = direct_owner_claim
        && state
            .nodes
            .get(&incoming_owner)
            .is_some_and(|node| node.is_serviceable());
    let peer_confirms_restarted_local_owner = direct_peer_id
        .and_then(|peer_node_id| state.nodes.get(peer_node_id))
        .is_some_and(|node| node.is_serviceable())
        && state
            .nodes
            .get(&incoming_owner)
            .is_some_and(|node| node.is_local && node.is_serviceable());
    match state.active_owner.as_deref() {
        Some(local_owner) if local_owner == incoming_owner => {
            state.lease_until = state.lease_until.max(incoming_lease);
        }
        Some(_) => {
            if !auto_failover && directly_reported_owner_is_eligible {
                if state.forced_owner.as_deref() != Some(incoming_owner.as_str()) {
                    state.forced_owner = None;
                }
                state.active_owner = Some(incoming_owner);
                state.lease_until = incoming_lease;
            }
            // Automatic mode resolves conflicting views in the local election
            // pass. Remote wall-clock lease values are not ordering evidence.
        }
        None => {
            // After a process restart, the active node has no in-memory owner.
            // A healthy configured peer can restore the peer's existing view
            // that this local node is still active. Claims about a third node
            // still have to come directly from that owner.
            if directly_reported_owner_is_eligible || peer_confirms_restarted_local_owner {
                state.active_owner = Some(incoming_owner);
                state.lease_until = incoming_lease;
            }
        }
    }
}
