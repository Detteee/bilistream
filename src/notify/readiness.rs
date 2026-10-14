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
    let nodes = digest
        .nodes
        .iter()
        .map(|node| {
            let is_local = node.id == digest.local;
            let fresh = is_local || node.fresh;
            let flags = if is_local { local } else { slot_flags(node) };
            json!({
                "node_id": node.id,
                "is_local": is_local,
                "notify_capable": fresh && if is_local { local_capable } else { node.notify_capable },
                "notify_authority": fresh && if is_local { local_authority } else { node.notify_authority },
                "fresh": fresh,
                "appid_set": flags.0,
                "appsecret_set": flags.1,
                "openid_set": flags.2,
                "missing": missing(fresh, flags, writer_openid),
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
