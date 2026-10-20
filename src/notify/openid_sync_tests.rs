//! Batch 1b gate tests (Clerk): non-owner 409, partition resolution, unbind
//! propagation with retries, removed member refused, sealed at rest, no
//! leak. The failover test (bind on A, fail over to B, B sends) is in
//! `worker_tests.rs`.

use super::*;
use crate::notify::store::{
    accept_copy, export_copy, load_openid, write_openid, CopyState, OpenidChange, OpenidWrite,
};
use crate::notify::test_support::{fast_policy, MemCluster, TestStore};
use std::sync::Arc;

const OPENID: &str = "E4F0A1B2C3D4E5F60718293A4B5C6D7E";
const OTHER: &str = "0123456789ABCDEF0123456789ABCDEF";

fn temp() -> TestStore {
    TestStore::new("openid-sync")
}

fn bind(store: &Store, writer: &str, openid: &str, expected: Option<u64>) -> u64 {
    write_openid(
        store,
        OpenidWrite {
            change: OpenidChange::Set {
                value: zeroize::Zeroizing::new(openid.to_string()),
                via: "manual",
            },
            expected,
            writer: writer.into(),
            now: 1_791_702_240,
        },
    )
    .unwrap()
}

fn unbind(store: &Store, writer: &str, expected: u64) -> u64 {
    write_openid(
        store,
        OpenidWrite {
            change: OpenidChange::Clear,
            expected: Some(expected),
            writer: writer.into(),
            now: 1_791_702_300,
        },
    )
    .unwrap()
}

fn value(store: &Store) -> Option<String> {
    load_openid(store)
        .unwrap()
        .openid
        .map(|id| id.expose().to_string())
}

const FAST: RetryPolicy = fast_policy(5);

#[test]
fn a_non_owner_answers_409_naming_the_owner_and_writes_nothing() {
    let b = temp();
    let not_owner = || {
        crate::notify::writer_allows(
            Some(&crate::notify::NodeRef {
                node_id: "a".into(),
                is_local: false,
            }),
            "修改管理员 openid",
        )
    };
    let update = crate::webui::api::notify::UpdateNotify {
        admin_openid: Some(OPENID.into()),
        expected_openid_revision: Some(0),
        ..Default::default()
    };
    let error =
        crate::webui::api::notify::apply_update(&b.1, &update, "b", &not_owner).unwrap_err();
    let crate::webui::api::notify::UpdateError::Refused(refusal) = error else {
        panic!("expected a 409 refusal");
    };
    assert_eq!(refusal.data["code"], "not_owner");
    assert_eq!(refusal.data["owner"], "a");
    assert!(!refusal.message.contains(OPENID));
    assert_eq!(load_openid(&b.1).unwrap().revision, 0, "nothing written");
    // The unbind route refuses the same way.
    bind(&b.1, "a", OPENID, None);
    let error = crate::webui::api::notify::apply_unbind(&b.1, 1, "b", &not_owner).unwrap_err();
    let crate::webui::api::notify::UpdateError::Refused(refusal) = error else {
        panic!("expected a 409 refusal");
    };
    assert_eq!(refusal.data["code"], "not_owner");
    assert_eq!(value(&b.1).as_deref(), Some(OPENID), "still bound");
    // config.qq.admin_openid in a PUT is a 400, also for the owner.
    let legacy = crate::webui::api::notify::UpdateNotify {
        config: Some(serde_json::json!({"qq": {"admin_openid": OTHER}})),
        expected_revision: Some(0),
        ..Default::default()
    };
    let error =
        crate::webui::api::notify::apply_update(&b.1, &legacy, "b", &|| Ok(())).unwrap_err();
    let crate::webui::api::notify::UpdateError::Status(status, message) = error else {
        panic!("expected a 400");
    };
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(!message.contains(OTHER));
}

#[tokio::test]
async fn bind_on_the_writer_reaches_every_member() {
    let (a, b, c) = (temp(), temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone()), ("c", c.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(report.failed, Vec::<String>::new());
    for store in [&b.1, &c.1] {
        let state = load_openid(store).unwrap();
        assert_eq!(value(store).as_deref(), Some(OPENID));
        assert_eq!((state.revision, state.writer.as_str()), (1, "a"));
    }
    // Pushing again changes nothing (idempotent ack).
    let again = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(again.acked.len(), 2);
}

#[tokio::test]
async fn partition_writes_converge_on_the_greatest_revision_and_writer() {
    let (a, b, c) = (temp(), temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone()), ("c", c.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    // Partition: a and b both believe they are the writer and write rev 2.
    bind(&a.1, "a", OPENID, Some(1));
    bind(&b.1, "b", OTHER, Some(1));
    assert_eq!(load_openid(&a.1).unwrap().revision, 2);
    assert_eq!(load_openid(&b.1).unwrap().revision, 2);
    // Healed: both push, in either order; every member keeps (2, "b").
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    push_all(&b.1, &cluster.link("b"), FAST).await.unwrap();
    for store in [&a.1, &b.1, &c.1] {
        let state = load_openid(store).unwrap();
        assert_eq!((state.revision, state.writer.as_str()), (2, "b"));
        assert_eq!(value(store).as_deref(), Some(OTHER));
    }
    // A higher revision beats a greater writer id.
    bind(&a.1, "a", OPENID, Some(2));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    for store in [&a.1, &b.1, &c.1] {
        assert_eq!(value(store).as_deref(), Some(OPENID));
        assert_eq!(load_openid(store).unwrap().revision, 3);
    }
    // A stale copy is acked but not applied.
    let mut stale = export_copy(&b.1).unwrap();
    stale.revision = 1;
    stale.writer = "z".into();
    let outcome = accept_copy(&c.1, &stale).unwrap();
    assert!(!outcome.applied);
    assert_eq!(outcome.revision, 3);
}

#[tokio::test]
async fn unbind_clears_every_member_with_retries_and_an_offline_one_on_pull() {
    let (a, b, c, d) = (temp(), temp(), temp(), temp());
    let cluster = MemCluster::new(&[
        ("a", a.1.clone()),
        ("b", b.1.clone()),
        ("c", c.1.clone()),
        ("d", d.1.clone()),
    ]);
    bind(&a.1, "a", OPENID, Some(0));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    // c drops the first two pushes; d is offline for the whole unbind.
    cluster.set_down("c", 2);
    cluster.set_down("d", u32::MAX);
    assert_eq!(unbind(&a.1, "a", 1), 2);
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(report.failed, vec!["d".to_string()]);
    assert!(report.acked.contains(&"b".to_string()) && report.acked.contains(&"c".to_string()));
    assert_eq!(
        cluster.pushes.lock().unwrap()["c"],
        1 + 3,
        "retried until acked"
    );
    assert_eq!(
        cluster.pushes.lock().unwrap()["b"],
        2,
        "no retry once acked"
    );
    assert_eq!(cluster.pushes.lock().unwrap()["d"], FAST.attempts + 1);
    for store in [&a.1, &b.1, &c.1] {
        let state = load_openid(store).unwrap();
        assert!(!state.is_set());
        assert_eq!(state.revision, 2);
    }
    assert_eq!(value(&d.1).as_deref(), Some(OPENID), "offline: still stale");
    // d comes back and pulls once: the explicit unbound state is newer.
    cluster.set_down("d", 0);
    let copy = cluster
        .link("d")
        .pull(1, "a")
        .await
        .expect("a answers the pull");
    assert_eq!(copy.state, CopyState::Unbound);
    assert_eq!(copy.revision, 2);
    assert!(copy.openid.is_empty());
    assert!(pull_all(&d.1, &cluster.link("d")).await.unwrap());
    assert!(!load_openid(&d.1).unwrap().is_set());
    // And a stale bound copy from d can no longer resurrect it.
    let mut resurrect = export_copy(&a.1).unwrap();
    resurrect.state = CopyState::Bound;
    resurrect.openid = OPENID.into();
    resurrect.revision = 1;
    assert!(!accept_copy(&a.1, &resurrect).unwrap().applied);
    assert!(!load_openid(&a.1).unwrap().is_set());
}

#[tokio::test]
async fn a_removed_member_can_neither_push_nor_pull_and_loses_its_copy() {
    let (a, b, x) = (temp(), temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone()), ("x", x.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(value(&x.1).as_deref(), Some(OPENID));
    // x is removed (new membership revision); its departure transaction
    // deletes its copy.
    cluster.remove("x");
    x.1.transaction(crate::notify::store::forget_openid_tx)
        .unwrap();
    assert!(!load_openid(&x.1).unwrap().is_set());
    assert_eq!(load_openid(&x.1).unwrap().revision, 0);
    // Sending side: no member list, so nothing is sent.
    assert!(push_all(&x.1, &cluster.link("x"), FAST).await.is_err());
    assert!(pull_all(&x.1, &cluster.link("x")).await.is_err());
    // Receiving side: a signed call from x is refused by every member.
    let members = cluster.members.lock().unwrap().clone();
    let mut forged = export_copy(&a.1).unwrap();
    forged.openid = OTHER.into();
    forged.revision = 99;
    forged.writer = "x".into();
    let body = serde_json::to_vec(&forged).unwrap();
    assert_eq!(
        serve_push(&b.1, "x", &members, &body),
        Err(ServeRefusal::NotMember)
    );
    assert_eq!(
        serve_pull(&b.1, "x", &members, true, true).map(|_| ()),
        Err(ServeRefusal::NotMember)
    );
    assert_eq!(
        serve_push(&b.1, "", &members, &body),
        Err(ServeRefusal::NotMember)
    );
    assert_eq!(value(&b.1).as_deref(), Some(OPENID), "b unchanged");
    // The push to a removed node is dropped once the membership moves.
    bind(&a.1, "a", OTHER, Some(1));
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(report.acked, vec!["b".to_string()]);
    assert!(!load_openid(&x.1).unwrap().is_set());
    // A call made at the old membership revision is refused too.
    assert_eq!(
        cluster.link("a").push(1, "b", &forged).await,
        Err(LinkError::MembershipChanged)
    );
    // After a re-join, any member's copy wins over the reset record.
    *cluster.members.lock().unwrap() = vec!["a".into(), "b".into(), "x".into()];
    *cluster.revision.lock().unwrap() += 1;
    assert!(pull_all(&x.1, &cluster.link("x")).await.unwrap());
    assert_eq!(value(&x.1).as_deref(), Some(OTHER));
}

#[tokio::test]
async fn received_copies_are_sealed_at_rest_and_outside_backups() {
    let (a, b) = (temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(value(&b.1).as_deref(), Some(OPENID));
    // Checkpoint-free scan of every file the store wrote (db, wal, shm).
    for temp in [&a, &b] {
        let mut stack = vec![temp.0.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let bytes = std::fs::read(&path).unwrap();
                assert!(
                    !bytes.windows(OPENID.len()).any(|w| w == OPENID.as_bytes()),
                    "plain openid in {}",
                    path.display()
                );
            }
        }
    }
    b.1.write("config.json", crate::storage::tests::config_fixture())
        .unwrap();
    let backup = b.1.export_backup("synthetic backup password").unwrap();
    assert!(!String::from_utf8_lossy(&backup).contains(OPENID));
    let restored = temp();
    restored
        .1
        .restore_backup("synthetic backup password", &backup)
        .unwrap();
    assert!(!load_openid(&restored.1).unwrap().is_set());
}

#[tokio::test]
async fn the_value_leaks_nowhere_but_the_peer_copy() {
    let (a, b) = (temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    let copy = export_copy(&a.1).unwrap();
    let debug = format!("{copy:?} {:?}", load_openid(&a.1).unwrap());
    assert!(!debug.contains(OPENID), "{debug}");
    let members = vec!["a".to_string(), "b".to_string()];
    let ack = serve_push(&b.1, "a", &members, &serde_json::to_vec(&copy).unwrap()).unwrap();
    let ack_json = serde_json::to_string(&ack).unwrap();
    assert!(!ack_json.contains(OPENID), "{ack_json}");
    // Errors never echo a value: an invalid copy carrying one.
    let mut bad = export_copy(&a.1).unwrap();
    bad.openid = format!("{OPENID}!");
    bad.revision = 9;
    let error = accept_copy(&b.1, &bad).unwrap_err().to_string();
    assert!(!error.contains(OPENID), "{error}");
    let mut injected = serde_json::to_value(&copy).unwrap();
    injected["extra"] = serde_json::json!(OPENID);
    assert_eq!(
        serve_push(&b.1, "a", &members, &serde_json::to_vec(&injected).unwrap()),
        Err(ServeRefusal::Invalid)
    );
    // The browser-facing views on the receiver: booleans and revisions.
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert!(!format!("{report:?}").contains(OPENID));
    let view = crate::webui::api::notify::settings_view_for_tests(&b.1).unwrap();
    assert_eq!(view["openid_set"], true);
    assert!(!view.to_string().contains(OPENID), "{view}");
    // The peer routes are never reachable from a browser.
    assert!(crate::cluster::peer_call::browser_forbidden(
        "/api/cluster/v1/notify/openid"
    ));
    assert!(
        crate::cluster::peer_call::peer_route("POST", "/api/cluster/v1/notify/openid").is_some()
    );
    assert!(
        crate::cluster::peer_call::peer_route("GET", "/api/cluster/v1/notify/openid").is_some()
    );
    assert!(
        crate::cluster::peer_call::peer_route("DELETE", "/api/cluster/v1/notify/openid").is_none()
    );
    // The forwarded save has its own signable POST path; no PUT anywhere.
    assert!(crate::cluster::peer_call::browser_forbidden(
        "/api/cluster/v1/notify/credentials"
    ));
    assert!(
        crate::cluster::peer_call::peer_route("POST", "/api/cluster/v1/notify/credentials")
            .is_some()
    );
    assert!(
        crate::cluster::peer_call::peer_route("PUT", "/api/cluster/v1/notify/openid").is_none()
    );
}

fn mark(revision: u64, writer_local: bool, at: u64) -> crate::notify::worker::PullMark {
    crate::notify::worker::PullMark {
        revision,
        writer_local,
        at,
    }
}

#[test]
fn pull_triggers_on_join_restart_writer_and_every_ten_minutes() {
    use crate::notify::worker::{next_pull, PULL_EVERY_SECS};
    assert_eq!(PULL_EVERY_SECS, 600);
    let t0 = 1_791_702_000;
    assert_eq!(
        next_pull(None, 4, false, t0),
        Some(mark(4, false, t0)),
        "first step"
    );
    let last = Some(mark(4, false, t0));
    assert_eq!(next_pull(last, 4, false, t0 + 10), None);
    assert_eq!(
        next_pull(last, 5, false, t0 + 10),
        Some(mark(5, false, t0 + 10)),
        "joined"
    );
    assert_eq!(
        next_pull(last, 4, true, t0 + 10),
        Some(mark(4, true, t0 + 10)),
        "became writer"
    );
    assert_eq!(
        next_pull(Some(mark(4, true, t0)), 4, false, t0 + 10),
        None,
        "losing it pulls nothing"
    );
    // Controlled clock: nothing at 599 s, a pull at 600 s, then again 600 s later.
    assert_eq!(next_pull(last, 4, false, t0 + 599), None);
    let due = next_pull(last, 4, false, t0 + 600).expect("periodic pull");
    assert_eq!(due, mark(4, false, t0 + 600));
    assert_eq!(next_pull(Some(due), 4, false, t0 + 1199), None);
    assert!(next_pull(Some(due), 4, false, t0 + 1200).is_some());
    // A clock step backwards never pulls in a loop.
    assert_eq!(next_pull(last, 4, false, t0 - 5), None);
}

#[tokio::test]
async fn a_member_that_missed_every_retry_catches_up_on_the_periodic_pull() {
    use crate::notify::worker::next_pull;
    let (a, d) = (temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("d", d.1.clone())]);
    let t0 = 1_791_702_000;
    let mut last = next_pull(None, 1, false, t0);
    assert!(
        pull_all(&d.1, &cluster.link("d")).await.is_ok(),
        "start pull"
    );
    cluster.set_down("d", u32::MAX);
    bind(&a.1, "a", OPENID, Some(0));
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(report.failed, vec!["d".to_string()], "every retry missed");
    cluster.set_down("d", 0);
    assert!(!load_openid(&d.1).unwrap().is_set());
    // No restart, no membership change, no real waiting: only the clock moves.
    for now in [t0 + 60, t0 + 300, t0 + 599] {
        assert!(next_pull(last, 1, false, now).is_none());
    }
    last = next_pull(last, 1, false, t0 + 600);
    assert!(last.is_some());
    assert!(pull_all(&d.1, &cluster.link("d")).await.unwrap());
    assert_eq!(value(&d.1).as_deref(), Some(OPENID));
}

#[tokio::test]
async fn a_periodic_pull_of_an_older_bound_copy_never_beats_a_newer_unbind() {
    let (a, d, e) = (temp(), temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("d", d.1.clone()), ("e", e.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    // e misses the unbind entirely and keeps the old bound copy (rev 1).
    cluster.set_down("e", u32::MAX);
    unbind(&a.1, "a", 1);
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert!(!load_openid(&d.1).unwrap().is_set());
    cluster.set_down("e", 0);
    // d's 10-minute pull sees e's older bound copy (and a's unbound one).
    let stale = cluster.link("d").pull(1, "e").await.unwrap();
    assert_eq!((stale.state, stale.revision), (CopyState::Bound, 1));
    pull_all(&d.1, &cluster.link("d")).await.unwrap();
    let state = load_openid(&d.1).unwrap();
    assert!(!state.is_set(), "d stays unbound");
    assert_eq!(state.revision, 2);
    // Order does not matter: the older bound copy alone is still refused.
    assert!(!accept_copy(&d.1, &stale).unwrap().applied);
    assert!(!load_openid(&d.1).unwrap().is_set());
    // e's own periodic pull clears it (newer unbound revision wins).
    assert!(pull_all(&e.1, &cluster.link("e")).await.unwrap());
    assert!(!load_openid(&e.1).unwrap().is_set());
    assert_eq!(load_openid(&e.1).unwrap().revision, 2);
}

#[test]
fn the_openid_never_travels_over_plain_http_to_a_remote_address() {
    // Sending side.
    for url in [
        "http://203.0.113.5:3150",
        "http://example.com",
        "http://10.0.0.2:3150/",
        "http://[2001:db8::1]:3150",
        "ftp://127.0.0.1",
        "not a url",
    ] {
        assert!(!outbound_transport_allowed(url), "{url} must be refused");
    }
    for url in [
        "https://jp.example",
        "https://203.0.113.5:8443",
        "http://127.0.0.1:41001",
        "http://127.8.0.1:41001",
        "http://[::1]:41001",
        "http://localhost:3150",
    ] {
        assert!(outbound_transport_allowed(url), "{url} must be allowed");
    }
    // Receiving side: only a proxy's "came in as plain HTTP" is refused.
    let none = axum::http::HeaderMap::new();
    assert!(inbound_transport_allowed(&none), "signed, no proxy header");
    let mut https = axum::http::HeaderMap::new();
    https.insert("x-forwarded-proto", "https".parse().unwrap());
    https.insert("x-forwarded-for", "203.0.113.5".parse().unwrap());
    assert!(inbound_transport_allowed(&https), "TLS proxy");
    let mut plain = axum::http::HeaderMap::new();
    plain.insert("x-forwarded-proto", "http".parse().unwrap());
    assert!(
        !inbound_transport_allowed(&plain),
        "plain HTTP, unknown client"
    );
    plain.insert("x-forwarded-for", "203.0.113.5".parse().unwrap());
    assert!(!inbound_transport_allowed(&plain), "plain HTTP from remote");
    plain.insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
    assert!(
        !inbound_transport_allowed(&plain),
        "a loopback X-Forwarded-For changes nothing"
    );
}

fn node() -> Option<axum::Extension<crate::cluster::peer_call::AuthenticatedNode>> {
    Some(axum::Extension(
        crate::cluster::peer_call::AuthenticatedNode {
            member_id: "m".into(),
            node_id: "a".into(),
        },
    ))
}

async fn body_text(response: axum::response::Response) -> String {
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

/// `X-Forwarded-Proto: http` from a remote client is rejected on both
/// routes. The header is advisory (anyone can send it), so this only shows
/// the rejection; it is not a security boundary and enforces nothing.
#[tokio::test]
async fn x_forwarded_proto_http_from_a_remote_client_is_rejected() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-forwarded-proto", "http".parse().unwrap());
    headers.insert("x-forwarded-for", "203.0.113.5".parse().unwrap());
    let response = crate::webui::api::notify::peer_openid_push(
        node(),
        headers.clone(),
        axum::body::Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(body_text(response).await.contains("HTTPS"));
    let response = crate::webui::api::notify::peer_openid_pull(node(), headers).await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(body_text(response).await.contains("HTTPS"));
}

/// A forged loopback `X-Forwarded-For` next to `X-Forwarded-Proto: http`
/// is still rejected on both routes: forwarded headers never let a request
/// through. (Only the rejection is shown; the header is advisory.)
#[tokio::test]
async fn a_forged_loopback_x_forwarded_for_does_not_bypass_the_http_refusal() {
    for forged in ["127.0.0.1", "::1", "127.0.0.1, 203.0.113.5"] {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        headers.insert("x-forwarded-for", forged.parse().unwrap());
        assert!(!inbound_transport_allowed(&headers), "{forged}");
        let push = crate::webui::api::notify::peer_openid_push(
            node(),
            headers.clone(),
            axum::body::Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(push.status(), axum::http::StatusCode::FORBIDDEN, "{forged}");
        assert!(body_text(push).await.contains("HTTPS"));
        let pull = crate::webui::api::notify::peer_openid_pull(node(), headers).await;
        assert_eq!(pull.status(), axum::http::StatusCode::FORBIDDEN, "{forged}");
        assert!(body_text(pull).await.contains("HTTPS"));
    }
}

/// A signed request from a remote socket (a TLS proxy on another machine,
/// no `X-Forwarded-Proto: http`) passes the transport check on both routes.
/// The handlers take no socket address at all, so the remote origin cannot
/// matter; what answers is the membership/store step after the check.
#[tokio::test]
async fn a_signed_request_from_a_remote_tls_proxy_is_accepted_on_both_routes() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-forwarded-proto", "https".parse().unwrap());
    headers.insert("x-forwarded-for", "203.0.113.5".parse().unwrap());
    for headers in [headers, axum::http::HeaderMap::new()] {
        assert!(inbound_transport_allowed(&headers));
        let push = crate::webui::api::notify::peer_openid_push(
            node(),
            headers.clone(),
            axum::body::Bytes::from_static(b"{}"),
        )
        .await;
        assert!(
            !body_text(push).await.contains("HTTPS"),
            "push passed the transport check"
        );
        let pull = crate::webui::api::notify::peer_openid_pull(node(), headers).await;
        assert!(
            !body_text(pull).await.contains("HTTPS"),
            "pull passed the transport check"
        );
    }
}

/// N1 (rev 4): a membership change in the middle of an unbind. The push to
/// c hits the change (`MembershipChanged`), the next member reads fail as
/// "changing", and the new revision adds e. The round waits on its own
/// attempt budget and still clears every member, e included.
#[tokio::test]
async fn a_membership_change_in_the_middle_of_an_unbind_still_reaches_every_member() {
    let (a, b, c, d, e) = (temp(), temp(), temp(), temp(), temp());
    let cluster = MemCluster::new(&[
        ("a", a.1.clone()),
        ("b", b.1.clone()),
        ("c", c.1.clone()),
        ("d", d.1.clone()),
    ]);
    bind(&a.1, "a", OPENID, Some(0));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    // e joins with a stale bound copy (it pulled before the unbind).
    accept_copy(&e.1, &export_copy(&a.1).unwrap()).unwrap();
    assert_eq!(value(&e.1).as_deref(), Some(OPENID));
    assert_eq!(unbind(&a.1, "a", 1), 2);
    *cluster.change_on_push.lock().unwrap() = Some(("c".into(), 3, "e".into(), e.1.clone()));
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(report.failed, Vec::<String>::new());
    let mut acked = report.acked.clone();
    acked.sort();
    assert_eq!(acked, ["b", "c", "d", "e"]);
    for store in [&a.1, &b.1, &c.1, &d.1, &e.1] {
        let state = load_openid(store).unwrap();
        assert!(!state.is_set());
        assert_eq!(state.revision, 2);
    }
    assert_eq!(
        *cluster.changing.lock().unwrap(),
        0,
        "waited the change out"
    );
}

/// N1: a change that never settles within the budget is an error the caller
/// logs, not a silent stop; leaving the cluster mid-change stops at once.
#[tokio::test]
async fn an_unsettled_membership_is_reported_and_a_removed_writer_stops() {
    let (a, b) = (temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    *cluster.changing.lock().unwrap() = FAST.attempts;
    let error = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(!error.to_string().contains(OPENID));
    assert_eq!(value(&b.1), None, "nothing sent while changing");
    // Changing for two reads, then the change removed a: stop, send nothing.
    *cluster.changing.lock().unwrap() = 2;
    cluster.remove("a");
    let error = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(cluster.pushes.lock().unwrap().get("b"), None);
}

/// N2 (rev 4): binds and unbinds during a running round set one "push
/// again" flag; they merge into exactly one follow-up round, nothing
/// queues, and the follow-up sends the latest state.
#[tokio::test]
async fn binds_during_a_round_merge_into_one_bounded_follow_up_with_the_latest_state() {
    let (a, b) = (temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    let coalescer = PushCoalescer::new();
    let rounds = std::sync::atomic::AtomicU32::new(0);
    bind(&a.1, "a", OPENID, Some(0));
    assert!(coalescer.request(), "the first request starts the loop");
    coalescer
        .drive(|| async {
            let round = rounds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
            if round == 0 {
                // Fifty changes while the round runs: bind, unbind, rebind.
                let mut revision = load_openid(&a.1).unwrap().revision;
                for i in 0..50 {
                    revision = if i % 2 == 0 {
                        unbind(&a.1, "a", revision)
                    } else {
                        bind(&a.1, "a", OTHER, Some(revision))
                    };
                    assert!(!coalescer.request(), "merged, not queued");
                }
                assert_eq!(value(&b.1).as_deref(), Some(OPENID), "not yet sent");
            }
        })
        .await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::SeqCst), 2);
    // The last change (i = 49) was a bind of OTHER at revision 51.
    let state = load_openid(&b.1).unwrap();
    assert_eq!(state.revision, 51);
    assert_eq!(value(&b.1).as_deref(), Some(OTHER));
    assert_eq!(
        cluster.pushes.lock().unwrap()["b"],
        2,
        "two rounds, one push each"
    );
    // Idle again: the next request starts a new loop.
    assert!(coalescer.request());
    assert!(!coalescer.finish_round());
}

/// R4-1: on a single-node install a bind and an unbind skip the push
/// quietly: no warning (in particular not "本节点已不在当前成员清单") and
/// no peer calls, through both the production link and the test link.
#[tokio::test]
async fn a_single_node_bind_and_unbind_skip_the_push_without_a_warning_or_peer_calls() {
    use tracing::instrument::WithSubscriber;
    let a = temp();
    // Production link on a fresh, never-clustered store.
    let signed = SignedLink::new(Arc::clone(&a.1), "a".into());
    assert!(!signed.clustered());
    let cluster = MemCluster::new(&[("a", a.1.clone())]);
    *cluster.clustered.lock().unwrap() = false;
    let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
    async {
        let revision = bind(&a.1, "a", OPENID, Some(0));
        push_round(&a.1, &signed, FAST).await;
        push_round(&a.1, &cluster.link("a"), FAST).await;
        unbind(&a.1, "a", revision);
        push_round(&a.1, &signed, FAST).await;
        push_round(&a.1, &cluster.link("a"), FAST).await;
    }
    .with_subscriber(dispatch)
    .await;
    let text = logs.text();
    assert!(!text.contains("WARN"), "no warning: {text}");
    assert!(!text.contains("成员清单"));
    assert!(!text.contains(OPENID));
    assert_eq!(*cluster.member_reads.lock().unwrap(), 0);
    assert!(cluster.pushes.lock().unwrap().is_empty(), "zero peer calls");
    assert!(
        !load_openid(&a.1).unwrap().is_set(),
        "the local unbind stands"
    );
}

/// A panicking round clears the running flag, so a later bind still
/// starts a round instead of only setting "push again".
#[tokio::test]
async fn a_panicking_round_does_not_block_later_pushes() {
    let coalescer: &'static PushCoalescer = Box::leak(Box::new(PushCoalescer::new()));
    assert!(coalescer.request());
    // F2: the panic is caught inside `drive`; with no follow-up owed the
    // loop ends and leaves the coalescer idle.
    tokio::spawn(async move {
        coalescer
            .drive(|| async { panic!("synthetic round failure") })
            .await;
    })
    .await
    .unwrap();
    // A new bind after the crash starts a fresh loop that really runs.
    let (a, b) = (temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    bind(&a.1, "a", OPENID, Some(0));
    assert!(coalescer.request(), "running flag was reset");
    let rounds = std::sync::atomic::AtomicU32::new(0);
    coalescer
        .drive(|| async {
            rounds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            push_round(&a.1, &cluster.link("a"), FAST).await;
        })
        .await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(value(&b.1).as_deref(), Some(OPENID));
    // And a normal finish leaves it idle too.
    assert!(coalescer.request());
    assert!(!coalescer.finish_round());
}

/// Each way a forwarded save can fail to get an answer keeps its own kind
/// and panel code; only a call that got no answer is "unreachable". The
/// push/pull paths keep their old coarse `LinkError`.
#[test]
fn a_send_failure_keeps_its_kind_and_panel_code() {
    use crate::cluster::peer_auth::unsigned_error;
    use crate::notify::credentials::SaveError;
    let io = |kind| io::Error::new(kind, "x");
    let cases = [
        (
            SendFailure::from_io(&unsigned_error(401)),
            SendFailure::Unsigned(Some(401)),
            SaveError::HolderRefused { status: 401 },
            LinkError::Refused,
        ),
        (
            SendFailure::from_io(&unsigned_error(403)),
            SendFailure::Unsigned(Some(403)),
            SaveError::HolderRefused { status: 403 },
            LinkError::Refused,
        ),
        (
            SendFailure::from_io(&unsigned_error(502)),
            SendFailure::Unreachable(Some(502)),
            SaveError::HolderUnreachable,
            LinkError::Unreachable,
        ),
        (
            SendFailure::from_io(&io(io::ErrorKind::TimedOut)),
            SendFailure::Unreachable(None),
            SaveError::HolderUnreachable,
            LinkError::Unreachable,
        ),
        (
            SendFailure::from_io(&io(io::ErrorKind::ConnectionRefused)),
            SendFailure::NotConnected,
            SaveError::HolderNotConnected,
            LinkError::Unreachable,
        ),
        (
            SendFailure::from_io(&io(io::ErrorKind::WouldBlock)),
            SendFailure::MembershipChanged,
            SaveError::MembershipChanging,
            LinkError::MembershipChanged,
        ),
        (
            SendFailure::from_io(&io(io::ErrorKind::NotFound)),
            SendFailure::NotInManifest,
            SaveError::ForwardRefused,
            LinkError::Refused,
        ),
        (
            // A failed response signature (no status): not unreachable.
            SendFailure::from_io(&io(io::ErrorKind::PermissionDenied)),
            SendFailure::BadReply,
            SaveError::HolderBadReply,
            LinkError::Refused,
        ),
        (
            SendFailure::from_io(&io(io::ErrorKind::InvalidData)),
            SendFailure::BadReply,
            SaveError::HolderBadReply,
            LinkError::Refused,
        ),
        (
            SendFailure::Transport,
            SendFailure::Transport,
            SaveError::ForwardRefused,
            LinkError::Refused,
        ),
        (
            SendFailure::TooLarge,
            SendFailure::TooLarge,
            SaveError::ForwardRefused,
            LinkError::Refused,
        ),
    ];
    for (got, kind, save, link) in cases {
        assert_eq!(got, kind);
        assert_eq!(got.save_error(), save, "{}", got.kind());
        assert_eq!(got.link_error(), link, "{}", got.kind());
    }
    // `outcome` per sender branch: not_sent only when nothing left this node.
    for (failure, outcome) in [
        (SendFailure::MembershipChanged, "not_sent"),
        (SendFailure::NotInManifest, "not_sent"),
        (SendFailure::Transport, "not_sent"),
        (SendFailure::TooLarge, "not_sent"),
        (SendFailure::RouteInvalid, "not_sent"),
        (SendFailure::Client, "not_sent"),
        (SendFailure::NotConnected, "not_sent"),
        (SendFailure::Unsigned(Some(401)), "not_sent"),
        (SendFailure::Unsigned(Some(500)), "unknown"),
        (SendFailure::Unreachable(None), "unknown"),
        (SendFailure::Unreachable(Some(524)), "unknown"),
        (SendFailure::BadReply, "unknown"),
    ] {
        assert_eq!(
            failure.save_error().forward_outcome(),
            outcome,
            "{}",
            failure.kind()
        );
    }
    // Holder answers read by `forward_reply`.
    for (status, body, outcome) in [
        (
            409,
            &br#"{"data":{"code":"credentials_conflict"}}"#[..],
            "not_sent",
        ),
        (409, br#"{"data":{"code":"watch_conflict"}}"#, "not_sent"),
        (
            400,
            br#"{"data":{"code":"credentials_invalid"}}"#,
            "not_sent",
        ),
        (403, b"", "not_sent"),
        (500, br#"{"data":{"code":"credentials_store"}}"#, "unknown"),
        (500, b"", "unknown"),
        (200, b"garbage", "unknown"),
    ] {
        assert_eq!(
            forward_reply(status, body).unwrap_err().forward_outcome(),
            outcome,
            "{status}"
        );
    }
    // The transport rule that yields `Transport`: plain HTTP to a remote host.
    assert!(!outbound_transport_allowed("http://jp.example.test"));
    assert!(outbound_transport_allowed("https://jp.example.test"));
    assert!(outbound_transport_allowed("http://127.0.0.1:3150"));
}

/// The forward warn line: route, node id, kind and status only.
#[test]
fn the_forward_warn_line_carries_only_route_node_kind_and_status() {
    let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
    tracing::dispatcher::with_default(&dispatch, || {
        log_forward_failure("jp", SendFailure::Unsigned(Some(401)).kind(), Some(401));
        log_forward_failure("jp", SendFailure::Unreachable(None).kind(), None);
        let upstream = SendFailure::from_io(&crate::cluster::peer_auth::unsigned_error(524));
        log_forward_failure("jp", upstream.kind(), upstream.status());
    });
    let text = logs.text();
    assert!(
        text.contains("jp") && text.contains("route=notify_credentials_save"),
        "{text}"
    );
    assert!(text.contains("kind=unsigned_refusal status=401"), "{text}");
    assert!(text.contains("kind=unreachable"), "{text}");
    // L1: an upstream gateway status (e.g. Cloudflare 524) is logged.
    assert!(text.contains("kind=unreachable status=524"), "{text}");
    assert!(!text.contains("http"), "{text}");
}
