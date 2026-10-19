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
    /// is the notification sender. Needs a quorum; user-facing messages name
    /// the sender without distinguishing it from an execution-owner sender.
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

/// The cluster-history recorder uses the execution election without QQ
/// credentials, enabled switches, gateway leases or per-bot generations.
/// During a quorum gap the lowest fresh, non-draining member records only
/// its observed partition after the same grace used by the notifier.
pub fn recorder(
    digest: &ClusterDigest,
    membership_blocked: bool,
    now: u64,
    last_quorum_seen: u64,
    grace_secs: u64,
) -> Authority {
    if membership_blocked || digest.handoff_pending {
        return Authority::None;
    }
    if execution_primary(digest, membership_blocked) {
        return Authority::Primary;
    }
    if !digest.enabled
        || digest.quorum
        || digest.peer_count == 0
        || digest.local_draining
        || now.saturating_sub(last_quorum_seen) < grace_secs
    {
        return Authority::None;
    }
    let lowest = digest
        .nodes
        .iter()
        .filter(|node| !node.draining)
        .filter(|node| node.id == digest.local || node.fresh)
        .filter(|node| digest.members.is_empty() || digest.members.contains(&node.id))
        .map(|node| node.id.as_str())
        .min();
    if lowest == Some(digest.local.as_str()) {
        Authority::Fallback
    } else {
        Authority::None
    }
}

/// The worker's committed local value overrides its older heartbeat copy.
pub fn latest_generation(digest: &ClusterDigest, local_generation: u64) -> u64 {
    digest
        .nodes
        .iter()
        .filter(|node| node.id != digest.local && node.fresh)
        .filter_map(|node| node.credentials_generation)
        .fold(local_generation, u64::max)
}

/// Unknown is a legacy peer; a known older generation is never eligible.
pub fn credentials_current(node: &NodeSlot, latest: u64) -> bool {
    node.credentials_generation
        .is_none_or(|generation| generation >= latest)
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
    local_generation: u64,
) -> Option<(String, bool)> {
    let local = || Some((digest.local.clone(), true));
    if !digest.enabled {
        return local();
    }
    if common_writer_block(digest, membership_blocked).is_some() {
        return None;
    }
    let owner_id = digest.owner.as_deref()?;
    let latest = latest_generation(digest, local_generation);
    let local_current = local_generation >= latest;
    let local_primary = execution_primary(digest, membership_blocked);
    let owner_capable = if local_primary {
        local_capable && local_current
    } else {
        let owner = digest.node(owner_id)?;
        if owner.id == digest.local || !owner.fresh || owner.draining {
            // Not executing here, and the owner is not a live peer.
            return None;
        }
        credentials_current(owner, latest) && (owner.notify_capable || owner.notify_authority)
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
                local_capable && local_current && !digest.local_draining
            } else {
                node.fresh && node.notify_capable && credentials_current(node, latest)
            }
        })
        .map(|node| node.id.as_str())
        .min();
    match delegate {
        Some(id) => Some((id.to_string(), id == digest.local)),
        None if local_primary && local_current => owner(),
        None if !local_primary
            && digest
                .node(owner_id)
                .is_some_and(|node| credentials_current(node, latest)) =>
        {
            owner()
        }
        None => None,
    }
}

fn common_writer_block(digest: &ClusterDigest, membership_blocked: bool) -> Option<&'static str> {
    if membership_blocked {
        Some("membership_change")
    } else if digest.enabled && !digest.quorum {
        Some("no_quorum")
    } else if digest.handoff_pending {
        Some("handoff")
    } else {
        None
    }
}

/// Stable, value-free diagnostics for a missing writer. A standby's stream
/// fence is expected and must never be reported as a notification failure.
pub fn sender_block_reason(
    digest: &ClusterDigest,
    membership_blocked: bool,
    writer_exists: bool,
) -> Option<&'static str> {
    if writer_exists {
        return None;
    }
    common_writer_block(digest, membership_blocked).or_else(|| {
        let owner_available = digest.owner.as_ref().is_some_and(|id| {
            id == &digest.local
                || digest
                    .node(id)
                    .is_some_and(|node| node.fresh && !node.draining)
        });
        Some(if owner_available {
            "execution_blocked"
        } else {
            "owner_unavailable"
        })
    })
}

pub struct DecideInput<'a> {
    pub digest: &'a ClusterDigest,
    pub membership_blocked: bool,
    /// Enabled, AppID + AppSecret + admin openid present on this node.
    pub capable: bool,
    pub local_generation: u64,
    pub strict_single_sender: bool,
    pub no_quorum_grace_secs: u64,
    /// Latest time this node saw any node (itself included) with authority.
    pub last_authority_seen: u64,
}

pub fn decide(input: &DecideInput<'_>) -> Authority {
    let digest = input.digest;
    let latest = latest_generation(digest, input.local_generation);
    if !input.capable || input.local_generation < latest {
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
        .filter(|node| {
            node.id == digest.local
                || (node.fresh && node.notify_capable && credentials_current(node, latest))
        })
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
    let latest = latest_generation(digest, input.local_generation);
    if !digest.enabled
        || !digest.quorum
        || input.membership_blocked
        || digest.local_draining
        || digest.handoff_pending
    {
        return None;
    }
    let owner = digest.node(digest.owner.as_deref()?)?;
    if owner.id == digest.local
        || !owner.fresh
        || owner.draining
        || (credentials_current(owner, latest) && (owner.notify_capable || owner.notify_authority))
    {
        return None;
    }
    let lowest = digest
        .nodes
        .iter()
        .filter(|node| node.id != owner.id && !node.draining)
        .filter(|node| {
            node.id == digest.local
                || (node.fresh && node.notify_capable && credentials_current(node, latest))
        })
        .map(|node| node.id.as_str())
        .min()?;
    (lowest == digest.local).then_some(Authority::Delegate)
}

/// Does any fresh peer currently advertise authority?
pub fn peer_has_authority(digest: &ClusterDigest, local_generation: u64) -> bool {
    let latest = latest_generation(digest, local_generation);
    digest.nodes.iter().any(|node| {
        node.id != digest.local
            && node.fresh
            && node.notify_authority
            && credentials_current(node, latest)
    })
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
                appsecret_set: node.notify_appsecret_set,
                appid_set: node.notify_appid_set,
                openid_set: node.notify_openid_set,
                credentials_generation: node.notify_credentials_generation,
                openid_rebind: node.notify_openid_rebind,
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
        // A normal demotion permanently holds streaming execution until
        // promotion. That standby may still be the QQ delegate. Only an
        // owner awaiting activation, or an actual pending handoff, blocks
        // notification authority; the streaming fence remains unchanged.
        handoff_pending: state.pending_handoff_source.is_some()
            || (state.local_execution_held
                && state.active_owner.as_deref() == Some(cfg.cluster.node_id.as_str())),
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
            local_generation: 0,
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

    fn demoted_cluster(local: &str, now: u64) -> Config {
        use crate::cluster::{empty_node, ClusterHealth, ClusterState, PeerOwnerView};
        let mut cfg = crate::test_support::config(local, 0);
        let members = ["ca", "jp", "ny"];
        cfg.cluster.peers = members
            .iter()
            .filter(|id| **id != local)
            .map(|id| crate::config::ClusterPeer {
                node_id: (*id).into(),
                name: (*id).into(),
                api_url: "http://127.0.0.1:9".into(),
                priority: 0,
            })
            .collect();
        let mut state = ClusterState {
            active_owner: Some("ca".into()),
            // This is the lasting state written by an ordinary demotion.
            local_execution_held: local != "ca",
            ..Default::default()
        };
        for id in members {
            let mut snapshot = empty_node(id, id, "", 0, id == local, now);
            snapshot.last_seen = Some(now);
            snapshot.health = ClusterHealth::healthy();
            snapshot.notify_capable = id == "jp";
            snapshot.notify_gateway_held = Some(false);
            state.nodes.insert(id.into(), snapshot);
            if id != local {
                state.peer_heartbeat_acks.insert(id.into(), now);
                state.peer_owner_views.insert(
                    id.into(),
                    PeerOwnerView {
                        owner: Some("ca".into()),
                        received_at: now,
                        members: members.iter().map(|id| (*id).to_string()).collect(),
                        confirmed_by_heartbeat: true,
                    },
                );
            }
        }
        *crate::cluster::cluster_state_write() = state;
        cfg
    }

    #[test]
    fn a_normally_demoted_owner_stays_stream_fenced_but_is_the_unique_notification_delegate() {
        let _guard = crate::cluster::tests::ClusterStateGuard::new();
        let now = 10_000;
        let mut senders = Vec::new();
        for local in ["ca", "jp", "ny"] {
            let cfg = demoted_cluster(local, now);
            let digest = read_digest(&cfg, now);
            assert!(digest.quorum);
            assert!(!digest.handoff_pending);
            let capable = local == "jp";
            senders.push(run(&digest, now, false, capable));
            let writer = openid_writer(&digest, false, capable, 0);
            assert_eq!(writer, Some(("jp".into(), capable)));
            assert_eq!(sender_block_reason(&digest, false, writer.is_some()), None);
            if local != "ca" {
                assert!(
                    digest.fence_block.is_some(),
                    "standby must remain stream-fenced"
                );
                assert!(crate::cluster::cluster_state_read().local_execution_held);
            }
        }
        assert_eq!(
            senders,
            vec![Authority::None, Authority::Delegate, Authority::None]
        );

        let cfg = demoted_cluster("jp", now);
        crate::cluster::cluster_state_write()
            .nodes
            .get_mut("ca")
            .unwrap()
            .notify_gateway_held = Some(true);
        assert_eq!(
            may_identify(&read_digest(&cfg, now)),
            Err("ca".into()),
            "old gateway must close first"
        );
    }

    #[test]
    fn a_real_pending_handoff_and_lost_quorum_still_block_the_demoted_delegate() {
        let _guard = crate::cluster::tests::ClusterStateGuard::new();
        let now = 10_000;
        let cfg = demoted_cluster("jp", now);
        for (owner, held, source) in [
            ("jp", true, None),
            ("jp", false, Some("ca")),
            ("ca", true, Some("ny")),
        ] {
            {
                let mut state = crate::cluster::cluster_state_write();
                state.active_owner = Some(owner.into());
                state.local_execution_held = held;
                state.pending_handoff_source = source.map(str::to_string);
            }
            let digest = read_digest(&cfg, now);
            assert!(digest.quorum);
            assert!(digest.handoff_pending);
            assert_eq!(run(&digest, now, false, true), Authority::None);
            assert_eq!(openid_writer(&digest, false, true, 0), None);
            assert_eq!(sender_block_reason(&digest, false, false), Some("handoff"));
        }
        let cfg = demoted_cluster("jp", now);
        crate::cluster::cluster_state_write()
            .peer_heartbeat_acks
            .clear();
        let digest = read_digest(&cfg, now);
        assert!(!digest.quorum);
        assert_eq!(run(&digest, now, false, true), Authority::None);
        assert_eq!(openid_writer(&digest, false, true, 0), None);
        assert_eq!(
            sender_block_reason(&digest, false, false),
            Some("no_quorum")
        );
        assert_eq!(
            sender_block_reason(&digest, true, false),
            Some("membership_change")
        );
    }

    #[test]
    fn a_fresh_peer_with_authority_is_seen() {
        let mut d = digest("b", vec![node("a", true), node("b", true)], Some("x"));
        assert!(!peer_has_authority(&d, 0));
        d.nodes[0].notify_authority = true;
        assert!(peer_has_authority(&d, 0));
        d.nodes[0].fresh = false;
        assert!(!peer_has_authority(&d, 0));
    }

    #[test]
    fn known_older_generations_cannot_block_the_current_delegate_but_legacy_peers_remain_eligible()
    {
        let mut senders = Vec::new();
        for local in ["a", "b", "c"] {
            let mut nodes = vec![node("a", true), node("b", true), node("c", true)];
            for slot in &mut nodes {
                slot.credentials_generation = Some(if slot.id == "c" { 3 } else { 2 });
            }
            nodes[0].notify_authority = true;
            if local == "c" {
                // The local commit is newer than its last heartbeat.
                nodes[2].credentials_generation = Some(1);
            }
            let d = digest(local, nodes, (local != "a").then_some("standby"));
            let generation = if local == "c" { 3 } else { 2 };
            assert_eq!(
                openid_writer(&d, false, true, generation),
                Some(("c".into(), local == "c"))
            );
            assert!(!peer_has_authority(&d, generation));
            senders.push(decide(&DecideInput {
                digest: &d,
                membership_blocked: false,
                capable: true,
                local_generation: generation,
                strict_single_sender: false,
                no_quorum_grace_secs: 120,
                last_authority_seen: d.now,
            }));
        }
        assert_eq!(
            senders,
            vec![Authority::None, Authority::None, Authority::Delegate]
        );

        let mut nodes = vec![node("a", true), node("b", true), node("c", true)];
        nodes[0].credentials_generation = Some(2);
        nodes[1].credentials_generation = None;
        let mut d = digest("c", nodes, Some("standby"));
        assert_eq!(openid_writer(&d, false, true, 3), Some(("b".into(), false)));
        d.nodes[0].draining = true;
        assert_eq!(openid_writer(&d, false, true, 3), None);
        assert_eq!(
            decide(&DecideInput {
                digest: &d,
                membership_blocked: false,
                capable: true,
                local_generation: 3,
                strict_single_sender: false,
                no_quorum_grace_secs: 120,
                last_authority_seen: d.now,
            }),
            Authority::None
        );
    }
}
