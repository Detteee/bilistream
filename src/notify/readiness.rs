//! Addendum C: per-node readiness for `/status` and `/api/notify`.
//! Booleans and fixed codes only; no value of the AppID, AppSecret or
//! openid is ever read here.

use super::observer::{ClusterDigest, NodeSlot};
use super::NodeRef;
use serde_json::{json, Value};

/// `(appid_set, appsecret_set, openid_set)` of this node.
pub type LocalFlags = (bool, bool, bool);

/// The sending node's own `openid_set`. `None` (field absent) when there
/// is no writer, or the writer is a peer whose heartbeat does not carry the
/// flag (older build) or is stale.
pub fn writer_openid_set(
    writer: Option<&NodeRef>,
    local: LocalFlags,
    nodes: &[NodeSlot],
) -> Option<bool> {
    let writer = writer?;
    if writer.is_local {
        return Some(local.2);
    }
    nodes
        .iter()
        .find(|node| node.id == writer.node_id && node.fresh)
        .and_then(|node| node.openid_set)
}

/// Fixed codes, in contract order: `appid`, `appsecret`, then
/// `openid_pending` (the writer has one, this node not yet) or `openid`
/// (the writer has none, or no writer is known). A stale node is only
/// `stale`. Empty = can send.
pub fn missing(fresh: bool, flags: LocalFlags, writer_openid: Option<bool>) -> Vec<&'static str> {
    if !fresh {
        return vec!["stale"];
    }
    let mut codes = Vec::new();
    if !flags.0 {
        codes.push("appid");
    }
    if !flags.1 {
        codes.push("appsecret");
    }
    if !flags.2 {
        codes.push(if writer_openid == Some(true) {
            "openid_pending"
        } else {
            "openid"
        });
    }
    codes
}

/// Addendum F view of one node: its set generation (`None` = older node)
/// and `openid_rebind`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncState {
    pub generation: Option<u64>,
    pub rebind: bool,
}

/// What the cluster as a whole has: the greatest generation and whether
/// any fresh node has an AppID / AppSecret at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClusterHas {
    pub generation: u64,
    pub appid: bool,
    pub appsecret: bool,
}

/// Addendum F rule 6 on top of `missing`: `appid_pending` /
/// `appsecret_pending` when the cluster has the value but this node is
/// missing it or has not received this generation; `openid_rebind` when the
/// openid was bound under another AppID. `appid` / `appsecret` stay for "the
/// cluster has no value at all".
pub fn missing_synced(
    fresh: bool,
    flags: LocalFlags,
    writer_openid: Option<bool>,
    sync: SyncState,
    cluster: ClusterHas,
) -> Vec<&'static str> {
    if !fresh {
        return vec!["stale"];
    }
    let behind = sync.generation.is_some_and(|g| g < cluster.generation);
    let mut codes = Vec::new();
    if !flags.0 || behind {
        if cluster.appid && (behind || sync.generation.is_some()) {
            codes.push("appid_pending");
        } else if !flags.0 {
            codes.push("appid");
        }
    }
    if !flags.1 || behind {
        if cluster.appsecret && (behind || sync.generation.is_some()) {
            codes.push("appsecret_pending");
        } else if !flags.1 {
            codes.push("appsecret");
        }
    }
    if sync.rebind {
        codes.push("openid_rebind");
    } else if !flags.2 {
        codes.push(
            if writer_openid == Some(true) || (behind && cluster.generation > 0) {
                "openid_pending"
            } else {
                "openid"
            },
        );
    }
    codes
}

fn slot_flags(node: &NodeSlot) -> LocalFlags {
    (
        node.appid_set.unwrap_or(false),
        node.appsecret_set.unwrap_or(false),
        node.openid_set.unwrap_or(false),
    )
}

/// `nodes[]` and `writer_openid_set` for the views. `local_capable` /
/// `local_authority` come from the worker. A stale node never counts as
/// able to send or as the authority.
pub fn view(
    digest: &ClusterDigest,
    writer: Option<&NodeRef>,
    local: LocalFlags,
    local_capable: bool,
    local_authority: bool,
) -> (Vec<Value>, Option<bool>) {
    let writer_openid = if digest.enabled {
        writer_openid_set(writer, local, &digest.nodes)
    } else {
        writer.filter(|w| w.is_local).map(|_| local.2)
    };
    if !digest.enabled {
        return (Vec::new(), writer_openid);
    }
    let local_sync = {
        let (generation, rebind) = super::local_credentials_sync();
        SyncState {
            generation: Some(generation),
            rebind,
        }
    };
    let sync_of = |node: &NodeSlot| {
        if node.id == digest.local {
            local_sync
        } else {
            SyncState {
                generation: node.credentials_generation,
                rebind: node.openid_rebind.unwrap_or(false),
            }
        }
    };
    let mut cluster = ClusterHas {
        generation: super::authority::latest_generation(digest, local_sync.generation.unwrap_or(0)),
        ..Default::default()
    };
    for node in &digest.nodes {
        let is_local = node.id == digest.local;
        if !(is_local || node.fresh) {
            continue;
        }
        let flags = if is_local { local } else { slot_flags(node) };
        cluster.appid |= flags.0;
        cluster.appsecret |= flags.1;
        cluster.generation = cluster
            .generation
            .max(sync_of(node).generation.unwrap_or(0));
    }
    let nodes = digest
        .nodes
        .iter()
        .map(|node| {
            let is_local = node.id == digest.local;
            let fresh = is_local || node.fresh;
            let flags = if is_local { local } else { slot_flags(node) };
            let sync = sync_of(node);
            let current = if is_local {
                local_sync.generation.unwrap_or(0) >= cluster.generation
            } else {
                super::authority::credentials_current(node, cluster.generation)
            };
            json!({
                "node_id": node.id,
                "is_local": is_local,
                "notify_capable": fresh && current && if is_local { local_capable } else { node.notify_capable },
                "notify_authority": fresh && current && if is_local { local_authority } else { node.notify_authority },
                "fresh": fresh,
                "appid_set": flags.0,
                "appsecret_set": flags.1,
                "openid_set": flags.2,
                "credentials_generation": sync.generation,
                "missing": if cluster.generation == 0 {
                    missing(fresh, flags, writer_openid)
                } else {
                    missing_synced(fresh, flags, writer_openid, sync, cluster)
                },
            })
        })
        .collect();
    (nodes, writer_openid)
}

/// `openid_writer` as the worker's status shows it.
pub fn writer_from_status(status: &Value) -> Option<NodeRef> {
    let writer = status.get("openid_writer")?;
    Some(NodeRef {
        node_id: writer.get("node_id")?.as_str()?.to_string(),
        is_local: writer.get("is_local")?.as_bool()?,
    })
}

#[cfg(test)]
#[path = "readiness_tests.rs"]
mod tests;
