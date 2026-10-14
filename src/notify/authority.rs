//! One sender per event (design §8, review §7).
//!
//! The sender is the node the streaming fence already lets execute
//! (`state_monitoring_block_reason` is `None`: active owner, not drained, no
//! latched fault, no handoff hold, fresh quorum, owner agreement), with no
//! membership hold, and with complete notifier settings. Single-node mode is
//! always its own sender. When no node has had authority for the grace
//! period, the lowest fresh capable node sends, labelled （无多数派）.

use super::observer::{ClusterDigest, NodeSlot};
use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    Primary,
    /// The execution primary has no complete notifier settings (for example
    /// the openid was bound on the old owner): the lowest fresh capable node
    /// sends everything in its place, labelled （代发）. Needs a quorum.
    Delegate,
    /// No-quorum fallback: may only send the collapsed 「失去多数派」 message
    /// and its resolution.
    Fallback,
    None,
}

impl Authority {
    pub fn as_str(self) -> &'static str {
        match self {
            Authority::Primary => "primary",
            Authority::Delegate => "delegate",
            Authority::Fallback => "no_quorum_fallback",
            Authority::None => "none",
        }
    }
}

/// The node the fence lets execute. It owns the QQ gateway session (when
/// one is wanted) whether or not its notifier settings are complete, so
/// binding works before an openid exists.
pub fn execution_primary(digest: &ClusterDigest, membership_blocked: bool) -> bool {
    !membership_blocked && (!digest.enabled || digest.fence_block.is_none())
}

/// The single openid writer (contract "Single writer" / "Writer edge
/// cases", Architect 17:43 and 18:12): the node with SENDER AUTHORITY, the
/// same node `/status.authority_node_id` names.
/// - Cluster off: the local node.
/// - The fenced owner (execution primary, `state_monitoring_block_reason`)
///   while it is fully configured, or while no fully configured node exists
///   (a fresh cluster binds its first openid there; no openid or gateway is
///   needed for that).
/// - The delegate while one is sending for an owner without the openid
///   (H2 rule, mirrored from `delegate`): it holds the gateway, so it can bind.
/// - Null without quorum (the no-quorum fallback is never a writer), during
///   a handoff hold or a membership change.
///
/// `local_capable` is this node's own "fully configured"; peers come from
/// their heartbeats. Returns `(node_id, is_local)`.
pub fn openid_writer(
    digest: &ClusterDigest,
    membership_blocked: bool,
    local_capable: bool,
) -> Option<(String, bool)> {
    let local = || Some((digest.local.clone(), true));
    if !digest.enabled {
        return local();
    }
    if membership_blocked || !digest.quorum || digest.handoff_pending {
        return None;
    }
    let owner_id = digest.owner.as_deref()?;
    let local_primary = execution_primary(digest, membership_blocked);
    let owner_capable = if local_primary {
        local_capable
    } else {
        let owner = digest.node(owner_id)?;
        if owner.id == digest.local || !owner.fresh || owner.draining {
            // Not executing here, and the owner is not a live peer.
            return None;
        }
        owner.notify_capable || owner.notify_authority
    };
    let owner = || {
        if local_primary {
            local()
        } else {
            Some((owner_id.to_string(), false))
        }
    };
    if owner_capable {
        return owner();
    }
    // Owner without the openid: the lowest fully configured, non-draining
    // other node is the delegate (same set as `delegate`).
    let delegate = digest
        .nodes
        .iter()
        .filter(|node| node.id != owner_id && !node.draining)
        .filter(|node| {
            if node.id == digest.local {
                local_capable && !digest.local_draining
            } else {
                node.fresh && node.notify_capable
            }
        })
        .map(|node| node.id.as_str())
        .min();
    match delegate {
        Some(id) => Some((id.to_string(), id == digest.local)),
        None => owner(),
    }
}

pub struct DecideInput<'a> {
    pub digest: &'a ClusterDigest,
    pub membership_blocked: bool,
    /// Enabled, AppID + AppSecret + admin openid present on this node.
    pub capable: bool,
    pub strict_single_sender: bool,
    pub no_quorum_grace_secs: u64,
    /// Latest time this node saw any node (itself included) with authority.
    pub last_authority_seen: u64,
}

pub fn decide(input: &DecideInput<'_>) -> Authority {
    let digest = input.digest;
    if !input.capable {
        return Authority::None;
    }
    if execution_primary(digest, input.membership_blocked) {
        return Authority::Primary;
    }
    if let Some(authority) = delegate(input) {
        return authority;
    }
    // The fallback is for a node that really has no quorum. With a quorum
    // (owner without notifier settings, or a long handoff) nobody else
    // speaks: the alerts wait for a real sender.
    if !digest.enabled
        || digest.quorum
        || digest.peer_count == 0
        || input.strict_single_sender
        || input.membership_blocked
        || digest.local_draining
        || digest.now.saturating_sub(input.last_authority_seen) < input.no_quorum_grace_secs
    {
        return Authority::None;
    }
    let lowest = digest
        .nodes
        .iter()
        .filter(|node| node.id == digest.local || (node.fresh && node.notify_capable))
        .map(|node| node.id.as_str())
        .min();
    if lowest == Some(digest.local.as_str()) {
        Authority::Fallback
    } else {
        Authority::None
    }
}

/// Full-scope stand-in for a fresh primary that cannot send.
fn delegate(input: &DecideInput<'_>) -> Option<Authority> {
    let digest = input.digest;
    if !digest.enabled
        || !digest.quorum
        || input.membership_blocked
        || digest.local_draining
        || digest.handoff_pending
    {
        return None;
    }
    let owner = digest.node(digest.owner.as_deref()?)?;
    if owner.id == digest.local || !owner.fresh || owner.notify_capable || owner.notify_authority {
        return None;
    }
    let lowest = digest
        .nodes
        .iter()
        .filter(|node| node.id != owner.id && !node.draining)
        .filter(|node| node.id == digest.local || (node.fresh && node.notify_capable))
        .map(|node| node.id.as_str())
        .min()?;
    (lowest == digest.local).then_some(Authority::Delegate)
}

/// Does any fresh peer currently advertise authority?
pub fn peer_has_authority(digest: &ClusterDigest) -> bool {
    digest
        .nodes
        .iter()
        .any(|node| node.id != digest.local && node.fresh && node.notify_authority)
}

/// May this node IDENTIFY/RESUME a QQ gateway session now? (contract line
/// 70: the old holder closes before the new one identifies, so there are
/// never two sessions; QQ would kick the first.) Pure: decided from the
/// heartbeat view alone. `Err(node)` names the lowest-id peer that may
/// still hold a session.
///
/// A peer counts while it is a current member and fresh: a stale/fenced
/// heartbeat or one silent past the peer-silence threshold
/// (`failover_timeout_secs`, which `fresh` already applies) no longer
/// blocks. `gateway_held: Some(false)` clears it at once. `None` (an older
/// node that does not report it) is unknown: it is treated as holding
/// while it advertises notify authority, because an older node's gateway
/// followed its authority; without authority it never opens one.
pub fn may_identify(digest: &ClusterDigest) -> Result<(), String> {
    if !digest.enabled {
        return Ok(());
    }
    digest
        .nodes
        .iter()
        .filter(|node| node.id != digest.local)
        .filter(|node| digest.members.is_empty() || digest.members.contains(&node.id))
        .filter(|node| node.fresh && !node.stale)
        .filter(|node| node.gateway_held.unwrap_or(node.notify_authority))
        .map(|node| node.id.clone())
        .min()
        .map_or(Ok(()), Err)
}

/// Snapshot what the notifier needs from the cluster state, then drop the
/// guard. Never clones `ClusterState` (nodes carry full status snapshots).
pub fn read_digest(cfg: &Config, now: u64) -> ClusterDigest {
    if !cfg.cluster.enabled {
        return ClusterDigest {
            enabled: false,
            now,
            local: cfg.cluster.node_id.clone(),
            ..Default::default()
        };
    }
    let timeout = cfg.cluster.failover_timeout_secs.max(1);
    let state = crate::cluster::cluster_state_read();
    let nodes = state
        .nodes
        .values()
        .map(|node| {
            let local = node.node_id == cfg.cluster.node_id;
            NodeSlot {
                id: node.node_id.clone(),
                name: node.name.clone(),
                healthy: node.health.healthy,
                reason: node.health.reason.clone(),
                stale: node.health.stale,
                draining: node.draining,
                fresh: local
                    || (!node.health.stale
                        && node
                            .last_seen
                            .is_some_and(|seen| now.saturating_sub(seen) <= timeout)),
                self_check: node
                    .self_check
                    .as_ref()
                    .map(|check| (check.state.clone(), check.consecutive_failures)),
                notify_capable: node.notify_capable,
                notify_authority: node.notify_authority,
                gateway_held: node.notify_gateway_held,
            }
        })
        .collect();
    let live = state
        .nodes
        .values()
        .filter(|node| {
            node.node_id != cfg.cluster.node_id
                && !node.health.stale
                && node
                    .last_seen
                    .is_some_and(|seen| now.saturating_sub(seen) <= timeout)
        })
        .filter_map(|node| node.status.as_ref())
        .flat_map(super::live::observations)
        .collect();
    let peer_ledgers = state
        .nodes
        .values()
        .filter(|node| node.node_id != cfg.cluster.node_id)
        .filter_map(|node| node.notify.clone())
        .collect();
    let digest = ClusterDigest {
        enabled: true,
        peer_ledgers,
        now,
        local: cfg.cluster.node_id.clone(),
        owner: state.active_owner.clone(),
        quorum: crate::cluster::state_has_fresh_quorum(&state, cfg, now),
        fence_block: crate::cluster::state_monitoring_block_reason(&state, cfg, now),
        handoff_pending: state.local_execution_held || state.pending_handoff_source.is_some(),
        local_draining: state.local_draining,
        peer_count: cfg
            .cluster
            .peers
            .iter()
            .filter(|peer| peer.node_id != cfg.cluster.node_id)
            .count(),
        members: std::iter::once(cfg.cluster.node_id.clone())
            .chain(cfg.cluster.peers.iter().map(|peer| peer.node_id.clone()))
            .collect(),
        live,
        nodes,
    };
    drop(state);
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, fresh: bool) -> NodeSlot {
        NodeSlot {
            id: id.into(),
            healthy: fresh,
            reason: if fresh {
                "healthy"
            } else {
                "heartbeat_timeout"
            }
            .into(),
            stale: !fresh,
            fresh,
            notify_capable: true,
            ..Default::default()
        }
    }

    fn digest(local: &str, nodes: Vec<NodeSlot>, block: Option<&str>) -> ClusterDigest {
        ClusterDigest {
            enabled: true,
            now: 10_000,
            local: local.into(),
            owner: Some("a".into()),
            quorum: block != Some("quorum"),
            fence_block: block.map(str::to_string),
            peer_count: nodes.len() - 1,
            nodes,
            ..Default::default()
        }
    }

    fn run(d: &ClusterDigest, last_seen: u64, strict: bool, capable: bool) -> Authority {
        decide(&DecideInput {
            digest: d,
            membership_blocked: false,
            capable,
            strict_single_sender: strict,
            no_quorum_grace_secs: 120,
            last_authority_seen: last_seen,
        })
    }

    #[test]
    fn three_nodes_only_the_unfenced_owner_sends() {
        let nodes = || vec![node("a", true), node("b", true), node("c", true)];
        let owner = digest("a", nodes(), None);
        let standby_b = digest("b", nodes(), Some("本节点已不是集群活跃节点"));
        let standby_c = digest("c", nodes(), Some("本节点已不是集群活跃节点"));
        let senders: Vec<_> = [&owner, &standby_b, &standby_c]
            .iter()
            .map(|d| run(d, 9_990, false, true))
            .collect();
        assert_eq!(
            senders,
            vec![Authority::Primary, Authority::None, Authority::None]
        );
    }

    #[test]
    fn fault_latched_owner_and_incapable_nodes_never_send() {
        let latched = digest(
            "a",
            vec![node("a", true), node("b", true)],
            Some("本节点已被判定故障 (network_isolated)"),
        );
        // Recently saw authority elsewhere: no fallback either.
        assert_eq!(run(&latched, 9_990, false, true), Authority::None);
        let owner = digest("a", vec![node("a", true), node("b", true)], None);
        assert_eq!(run(&owner, 0, false, false), Authority::None);
    }

    #[test]
    fn single_node_mode_is_its_own_sender() {
        let d = ClusterDigest {
            enabled: false,
            local: "solo".into(),
            ..Default::default()
        };
        assert_eq!(run(&d, 0, false, true), Authority::Primary);
        assert!(execution_primary(&d, false));
    }

    #[test]
    fn owner_death_waits_for_the_new_agreement() {
        // b and c lost the owner a; until they agree, neither is primary and
        // the grace period has not passed.
        let nodes = || vec![node("a", false), node("b", true), node("c", true)];
        let b = digest(
            "b",
            nodes(),
            Some("集群活跃节点存在分歧或尚未获得多数节点确认"),
        );
        assert_eq!(run(&b, 9_950, false, true), Authority::None);
        let mut b_owner = digest("b", nodes(), None);
        b_owner.owner = Some("b".into());
        assert_eq!(run(&b_owner, 9_950, false, true), Authority::Primary);
    }

    #[test]
    fn two_node_survivor_uses_the_fallback_after_grace() {
        let d = digest("b", vec![node("a", false), node("b", true)], Some("quorum"));
        assert_eq!(run(&d, 9_950, false, true), Authority::None);
        assert_eq!(run(&d, 9_800, false, true), Authority::Fallback);
        assert_eq!(run(&d, 9_800, true, true), Authority::None);
    }

    #[test]
    fn partition_sends_at_most_one_per_side() {
        // a | b c: a alone on its side; b and c see each other but no quorum
        // in a 4-node cluster (d is dead).
        let side_a = digest(
            "a",
            vec![
                node("a", true),
                node("b", false),
                node("c", false),
                node("d", false),
            ],
            Some("quorum"),
        );
        let bc = || {
            vec![
                node("a", false),
                node("b", true),
                node("c", true),
                node("d", false),
            ]
        };
        let side_b = digest("b", bc(), Some("quorum"));
        let side_c = digest("c", bc(), Some("quorum"));
        let results: Vec<_> = [&side_a, &side_b, &side_c]
            .iter()
            .map(|d| run(d, 0, false, true))
            .collect();
        assert_eq!(
            results,
            vec![Authority::Fallback, Authority::Fallback, Authority::None]
        );
    }

    #[test]
    fn no_fallback_while_a_quorum_exists() {
        // The owner a is fresh and capable but has not advertised authority
        // for a while (a long handoff): b keeps quiet even after the grace.
        let mut d = digest(
            "b",
            vec![node("a", true), node("b", true), node("c", true)],
            Some("本节点已不是集群活跃节点"),
        );
        assert!(d.quorum);
        assert_eq!(run(&d, 0, false, true), Authority::None);
        // Without a quorum (a unreachable) the fallback may start.
        d.quorum = false;
        d.nodes[0].fresh = false;
        assert_eq!(run(&d, 0, false, true), Authority::Fallback);
    }

    #[test]
    fn an_incapable_primary_gets_exactly_one_delegate() {
        let nodes = || {
            let mut nodes = vec![node("a", true), node("b", true), node("c", true)];
            nodes[0].notify_capable = false;
            nodes
        };
        let owner = digest("a", nodes(), None);
        let b = digest("b", nodes(), Some("本节点已不是集群活跃节点"));
        let c = digest("c", nodes(), Some("本节点已不是集群活跃节点"));
        let results: Vec<_> = [(&owner, false), (&b, true), (&c, true)]
            .iter()
            .map(|(d, capable)| run(d, 9_990, false, *capable))
            .collect();
        assert_eq!(
            results,
            vec![Authority::None, Authority::Delegate, Authority::None]
        );
        // Once the owner is capable again, the delegate stands down.
        let mut b2 = digest("b", nodes(), Some("x"));
        b2.nodes[0].notify_capable = true;
        assert_eq!(run(&b2, 9_990, false, true), Authority::None);
    }

    #[test]
    fn a_fresh_peer_with_authority_is_seen() {
        let mut d = digest("b", vec![node("a", true), node("b", true)], Some("x"));
        assert!(!peer_has_authority(&d));
        d.nodes[0].notify_authority = true;
        assert!(peer_has_authority(&d));
        d.nodes[0].fresh = false;
        assert!(!peer_has_authority(&d));
    }
}
