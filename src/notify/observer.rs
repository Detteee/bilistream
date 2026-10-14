//! Cluster digest → current condition set (design §2–§3, review §0.2).
//!
//! Pure. The worker reads a small digest from the cluster state under a
//! read guard (see `authority::read_digest`) and asks this module which
//! conditions hold right now. Nothing here is edge-triggered.

use super::event::{reason_text, Condition, EventKind};
use crate::cluster::SelfCheckState;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeSlot {
    pub id: String,
    /// Configured display name (may be empty).
    pub name: String,
    pub healthy: bool,
    pub reason: String,
    pub stale: bool,
    pub draining: bool,
    /// Seen recently enough to count for the no-quorum fallback election.
    pub fresh: bool,
    pub self_check: Option<(SelfCheckState, u32)>,
    pub notify_capable: bool,
    pub notify_authority: bool,
    /// The node's heartbeat says it holds a QQ gateway session; `None` when
    /// it does not say (older node).
    pub gateway_held: Option<bool>,
    /// 1b: the peer's heartbeat booleans; `None` from an older node.
    pub appsecret_set: Option<bool>,
    pub appid_set: Option<bool>,
    pub openid_set: Option<bool>,
    /// Addendum F: the peer's set generation and `openid_rebind`; `None`
    /// from an older node.
    pub credentials_generation: Option<u64>,
    pub openid_rebind: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct ClusterDigest {
    pub enabled: bool,
    pub now: u64,
    pub local: String,
    pub owner: Option<String>,
    /// This node sees a fresh heartbeat majority.
    pub quorum: bool,
    /// `state_monitoring_block_reason` (owner, drain, fault latch, handoff
    /// hold, quorum, agreement). `None` means this node may execute.
    pub fence_block: Option<String>,
    pub handoff_pending: bool,
    pub local_draining: bool,
    pub peer_count: usize,
    pub nodes: Vec<NodeSlot>,
    /// Notify ledgers from peer heartbeats, joined into the local one.
    pub peer_ledgers: Vec<super::ledger::NotifyLedger>,
    /// Configured members (local + peers). Empty when unknown.
    pub members: Vec<String>,
    /// Watch-list candidates: channels fresh peers report live.
    pub live: Vec<super::live::LiveObservation>,
}

impl ClusterDigest {
    pub fn node(&self, id: &str) -> Option<&NodeSlot> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

/// Down reasons; anything else unhealthy (and not drain) is "degraded".
const DOWN_REASONS: [&str; 2] = ["heartbeat_timeout", "api_unreachable"];
const SELF_CHECK_MIN_FAILURES: u32 = 3;

/// Conditions that hold now. `ever_healthy` lists nodes that were healthy at
/// least once in this process, so a node that never answered since startup
/// (`waiting_for_heartbeat`) is not reported as down.
pub fn conditions(
    digest: &ClusterDigest,
    ever_healthy: &HashSet<String>,
) -> BTreeMap<String, Condition> {
    let mut out = BTreeMap::new();
    if !digest.enabled {
        return out;
    }
    if digest.peer_count > 0 && !digest.quorum {
        // Minority side: one message about this node, never "B and C are
        // down" when it is this node that is cut off (review §0.8).
        let unreachable: Vec<&str> = digest
            .nodes
            .iter()
            .filter(|node| {
                node.id != digest.local && !node.draining && (!node.healthy || node.stale)
            })
            .map(|node| node.id.as_str())
            .collect();
        let detail = if unreachable.is_empty() {
            "本节点失去多数派".to_string()
        } else {
            format!("无法联系 {}", unreachable.join("、"))
        };
        let cond = Condition::new(EventKind::QuorumLost, &digest.local).with_detail(detail);
        out.insert(cond.dedup_key(), cond);
        return out;
    }
    for node in &digest.nodes {
        if node.draining || node.reason == "draining" {
            continue;
        }
        if !node.healthy {
            let down = DOWN_REASONS.contains(&node.reason.as_str())
                || (node.reason == "waiting_for_heartbeat" && ever_healthy.contains(&node.id));
            let cond = if down {
                Condition::new(EventKind::NodeDown, &node.id).with_detail(reason_text(&node.reason))
            } else if node.reason == "waiting_for_heartbeat" {
                continue;
            } else {
                Condition::new(EventKind::NodeDegraded, &node.id)
                    .with_variant(&node.reason)
                    .with_detail(reason_text(&node.reason))
            };
            out.insert(cond.dedup_key(), cond);
            if down {
                continue;
            }
        }
        if let Some((state, failures)) = &node.self_check {
            let failures = *failures;
            if matches!(state, SelfCheckState::Failing | SelfCheckState::Unreachable)
                && failures >= SELF_CHECK_MIN_FAILURES
            {
                let cond = Condition::new(EventKind::SelfCheckFailing, &node.id)
                    .with_detail(format!("连续 {failures} 次无法访问本节点公网地址"));
                out.insert(cond.dedup_key(), cond);
            }
        }
    }
    out
}

/// A failover: the owner changed and the previous owner is now unhealthy
/// (not drained by the operator). Returns `(old, new, reason)`.
pub fn failover(
    previous_owner: Option<&str>,
    digest: &ClusterDigest,
) -> Option<(String, String, String)> {
    let old = previous_owner?;
    let new = digest.owner.as_deref()?;
    if old == new || !digest.enabled {
        return None;
    }
    let slot = digest.node(old)?;
    if slot.draining || slot.reason == "draining" || (slot.healthy && !slot.stale) {
        return None;
    }
    Some((old.to_string(), new.to_string(), reason_text(&slot.reason)))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn slot(id: &str, healthy: bool, reason: &str) -> NodeSlot {
        NodeSlot {
            id: id.to_string(),
            healthy,
            reason: reason.to_string(),
            stale: reason == "heartbeat_timeout" || reason == "waiting_for_heartbeat",
            fresh: healthy,
            notify_capable: true,
            ..Default::default()
        }
    }

    fn digest(nodes: Vec<NodeSlot>) -> ClusterDigest {
        ClusterDigest {
            enabled: true,
            local: "a".into(),
            owner: Some("a".into()),
            quorum: true,
            peer_count: nodes.len().saturating_sub(1),
            nodes,
            ..Default::default()
        }
    }

    #[test]
    fn healthy_cluster_has_no_conditions() {
        let d = digest(vec![slot("a", true, "healthy"), slot("b", true, "healthy")]);
        assert!(conditions(&d, &HashSet::new()).is_empty());
    }

    #[test]
    fn heartbeat_timeout_is_down_and_fault_is_degraded() {
        let d = digest(vec![
            slot("a", true, "healthy"),
            slot("b", false, "heartbeat_timeout"),
            slot("c", false, "ffmpeg_repeated_failures"),
        ]);
        let c = conditions(&d, &HashSet::new());
        assert!(c.contains_key("cluster.node_down:b"));
        assert!(c.contains_key("cluster.node_degraded:c:ffmpeg_repeated_failures"));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn drain_and_never_seen_nodes_are_not_failures() {
        let mut drained = slot("b", false, "draining");
        drained.draining = true;
        let d = digest(vec![
            slot("a", true, "healthy"),
            drained,
            slot("c", false, "waiting_for_heartbeat"),
        ]);
        assert!(conditions(&d, &HashSet::new()).is_empty());
        let ever: HashSet<String> = ["c".to_string()].into();
        assert!(conditions(&d, &ever).contains_key("cluster.node_down:c"));
    }

    #[test]
    fn minority_collapses_into_one_condition() {
        let mut d = digest(vec![
            slot("a", true, "healthy"),
            slot("b", false, "heartbeat_timeout"),
            slot("c", false, "heartbeat_timeout"),
        ]);
        d.quorum = false;
        let c = conditions(&d, &HashSet::new());
        assert_eq!(c.len(), 1);
        let cond = &c["cluster.quorum_lost:a"];
        assert_eq!(cond.detail.as_deref(), Some("无法联系 b、c"));
    }

    #[test]
    fn self_check_needs_three_failures() {
        let mut a = slot("a", true, "healthy");
        a.self_check = Some((SelfCheckState::Failing, 2));
        let d = digest(vec![a.clone()]);
        assert!(conditions(&d, &HashSet::new()).is_empty());
        a.self_check = Some((SelfCheckState::Unreachable, 3));
        let d = digest(vec![a]);
        assert!(conditions(&d, &HashSet::new()).contains_key("cluster.self_check_failing:a"));
    }

    #[test]
    fn failover_needs_an_unhealthy_previous_owner() {
        let mut d = digest(vec![
            slot("a", false, "heartbeat_timeout"),
            slot("b", true, "healthy"),
        ]);
        d.owner = Some("b".into());
        assert_eq!(
            failover(Some("a"), &d),
            Some(("a".into(), "b".into(), "心跳超时".into()))
        );
        let mut manual = digest(vec![slot("a", true, "healthy"), slot("b", true, "healthy")]);
        manual.owner = Some("b".into());
        assert_eq!(failover(Some("a"), &manual), None);
        let mut drained = slot("a", false, "draining");
        drained.draining = true;
        let mut d = digest(vec![drained, slot("b", true, "healthy")]);
        d.owner = Some("b".into());
        assert_eq!(failover(Some("a"), &d), None);
        assert_eq!(failover(None, &d), None);
    }
}
