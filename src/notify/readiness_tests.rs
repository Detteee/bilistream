//! Addendum C gate tests.

use super::*;
use crate::notify::authority;

const OPENID: &str = "E4F0A1B2C3D4E5F60718293A4B5C6D7E";
const SECRET: &str = "synthetic-secret";
const APPID: &str = "1234567890";

fn slot(id: &str, fresh: bool, flags: (bool, bool, bool)) -> NodeSlot {
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
        notify_capable: flags.0 && flags.1 && flags.2,
        appid_set: Some(flags.0),
        appsecret_set: Some(flags.1),
        openid_set: Some(flags.2),
        ..Default::default()
    }
}

fn digest(local: &str, owner: &str, nodes: Vec<NodeSlot>) -> ClusterDigest {
    ClusterDigest {
        enabled: true,
        local: local.into(),
        owner: Some(owner.into()),
        quorum: true,
        fence_block: (local != owner).then(|| "本节点已不是集群活跃节点".to_string()),
        peer_count: nodes.len() - 1,
        members: nodes.iter().map(|n| n.id.clone()).collect(),
        nodes,
        ..Default::default()
    }
}

fn writer(id: &str, local: &str) -> NodeRef {
    NodeRef {
        node_id: id.into(),
        is_local: id == local,
    }
}

fn codes(nodes: &[Value], id: &str) -> Vec<String> {
    let node = nodes.iter().find(|n| n["node_id"] == id).unwrap();
    serde_json::from_value(node["missing"].clone()).unwrap()
}

const ALL: (bool, bool, bool) = (true, true, true);

#[test]
fn every_missing_code_in_contract_order() {
    assert_eq!(missing(true, ALL, Some(true)), Vec::<&str>::new());
    assert_eq!(missing(true, (false, true, true), Some(true)), ["appid"]);
    assert_eq!(
        missing(true, (true, false, true), Some(true)),
        ["appsecret"]
    );
    assert_eq!(
        missing(true, (true, true, false), Some(true)),
        ["openid_pending"]
    );
    assert_eq!(missing(true, (true, true, false), Some(false)), ["openid"]);
    assert_eq!(missing(true, (true, true, false), None), ["openid"]);
    assert_eq!(
        missing(true, (false, false, false), Some(true)),
        ["appid", "appsecret", "openid_pending"]
    );
    // Stale replaces everything, even a heartbeat that said all set.
    assert_eq!(missing(false, ALL, Some(true)), ["stale"]);
}

#[test]
fn openid_pending_only_while_the_writer_has_one_then_clears_on_sync_and_unbind_is_openid() {
    // tk is the writer with the openid; ny has AppID + AppSecret, no openid yet.
    let nodes = |ny_openid: bool, tk_openid: bool| {
        vec![
            slot("tk", true, (true, true, tk_openid)),
            slot("ny", true, (true, true, ny_openid)),
        ]
    };
    let local = (true, true, false);
    let (view_, set) = view(
        &digest("ny", "tk", nodes(false, true)),
        Some(&writer("tk", "ny")),
        local,
        false,
        false,
    );
    assert_eq!(set, Some(true));
    assert_eq!(codes(&view_, "ny"), ["openid_pending"]);
    assert_eq!(codes(&view_, "tk"), Vec::<String>::new());
    // The push lands: ny's own flags now say openid_set.
    let (view_, _) = view(
        &digest("ny", "tk", nodes(true, true)),
        Some(&writer("tk", "ny")),
        ALL,
        true,
        false,
    );
    assert_eq!(codes(&view_, "ny"), Vec::<String>::new());
    assert_eq!(view_[1]["openid_set"], true);
    // Writer unbinds (and the unbind reached ny): every node says "openid".
    let (view_, set) = view(
        &digest("ny", "tk", nodes(false, false)),
        Some(&writer("tk", "ny")),
        local,
        false,
        false,
    );
    assert_eq!(set, Some(false));
    assert_eq!(codes(&view_, "ny"), ["openid"]);
    assert_eq!(codes(&view_, "tk"), ["openid"]);
    // Seen from the writer itself.
    let (view_, set) = view(
        &digest("tk", "tk", nodes(false, true)),
        Some(&writer("tk", "tk")),
        ALL,
        true,
        true,
    );
    assert_eq!(set, Some(true));
    assert_eq!(codes(&view_, "ny"), ["openid_pending"]);
}

#[test]
fn appid_and_appsecret_codes_and_older_peers_report_false() {
    let mut old = slot("old", true, ALL);
    old.appid_set = None;
    old.appsecret_set = None;
    old.openid_set = None;
    let nodes = vec![
        slot("tk", true, ALL),
        slot("na", true, (false, true, true)),
        slot("nb", true, (true, false, false)),
        old,
    ];
    let (view_, set) = view(
        &digest("tk", "tk", nodes),
        Some(&writer("tk", "tk")),
        ALL,
        true,
        true,
    );
    assert_eq!(set, Some(true));
    assert_eq!(codes(&view_, "na"), ["appid"]);
    assert_eq!(codes(&view_, "nb"), ["appsecret", "openid_pending"]);
    let older = view_.iter().find(|n| n["node_id"] == "old").unwrap();
    assert_eq!(older["appid_set"], false);
    assert_eq!(older["appsecret_set"], false);
    assert_eq!(older["openid_set"], false);
}

#[test]
fn writer_openid_set_is_absent_without_a_writer_or_from_an_older_writer() {
    let nodes = vec![slot("tk", true, ALL), slot("ny", true, (true, true, false))];
    let d = digest("ny", "tk", nodes.clone());
    assert_eq!(view(&d, None, (true, true, false), false, false).1, None);
    let mut older = nodes.clone();
    older[0].openid_set = None;
    let d = digest("ny", "tk", older);
    let (view_, set) = view(
        &d,
        Some(&writer("tk", "ny")),
        (true, true, false),
        false,
        false,
    );
    assert_eq!(set, None);
    assert_eq!(codes(&view_, "ny"), ["openid"]);
    // Cluster off: the local node is the writer.
    let solo = ClusterDigest {
        local: "solo".into(),
        ..Default::default()
    };
    assert_eq!(
        view(&solo, Some(&writer("solo", "solo")), ALL, true, true).1,
        Some(true)
    );
}

#[test]
fn a_stale_node_is_stale_never_able_to_send_and_never_the_sender() {
    // ny's last heartbeat said fully configured, openid set, even authority.
    let mut ny = slot("ny", false, ALL);
    ny.notify_authority = true;
    ny.gateway_held = Some(false);
    let nodes = vec![slot("tk", true, (true, true, false)), ny.clone()];
    let (view_, _) = view(
        &digest("tk", "tk", nodes.clone()),
        Some(&writer("tk", "tk")),
        (true, true, false),
        false,
        true,
    );
    let stale = view_.iter().find(|n| n["node_id"] == "ny").unwrap();
    assert_eq!(codes(&view_, "ny"), ["stale"]);
    assert_eq!(stale["notify_capable"], false);
    assert_eq!(stale["notify_authority"], false);
    // Failover: tk owns but lacks the openid; the stale ny must not become
    // the delegate sender / writer / gateway holder.
    let d = digest("tk", "tk", nodes.clone());
    let picked = authority::openid_writer(&d, false, false, 0);
    assert_ne!(picked.as_ref().map(|(id, _)| id.as_str()), Some("ny"));
    // Seen from a third node: a stale owner with openid_set is no writer.
    let third = vec![
        slot("x", true, ALL),
        ny.clone(),
        slot("tk", true, (true, true, false)),
    ];
    let d = digest("x", "ny", third);
    assert_eq!(authority::openid_writer(&d, false, true, 0), None);
    // And a fresh delegate is picked instead of the stale one.
    let with_fresh = vec![
        slot("tk", true, (true, true, false)),
        ny,
        slot("zz", true, ALL),
    ];
    let d = digest("tk", "tk", with_fresh);
    assert_eq!(
        authority::openid_writer(&d, false, false, 0).map(|(id, _)| id),
        Some("zz".to_string())
    );
}

#[test]
fn no_secret_material_in_the_readiness_json() {
    let nodes = vec![
        slot("tk", true, ALL),
        slot("ny", true, (true, true, false)),
        slot("old", false, ALL),
    ];
    let (view_, set) = view(
        &digest("tk", "tk", nodes),
        Some(&writer("tk", "tk")),
        ALL,
        true,
        true,
    );
    let text = json!({"nodes": view_, "writer_openid_set": set}).to_string();
    for secret in [OPENID, SECRET, APPID, &OPENID[..8], &SECRET[..8]] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    let allowed = [
        "node_id",
        "is_local",
        "notify_capable",
        "notify_authority",
        "fresh",
        "appid_set",
        "appsecret_set",
        "openid_set",
        "missing",
        // Addendum F rule 4: per-node sync state (a number, no value).
        "credentials_generation",
    ];
    for node in &view_ {
        for key in node.as_object().unwrap().keys() {
            assert!(allowed.contains(&key.as_str()), "{key}");
        }
        for code in node["missing"].as_array().unwrap() {
            assert!([
                "appid",
                "appsecret",
                "openid_pending",
                "openid",
                "stale",
                "appid_pending",
                "appsecret_pending",
                "openid_rebind"
            ]
            .contains(&code.as_str().unwrap()));
        }
    }
}
