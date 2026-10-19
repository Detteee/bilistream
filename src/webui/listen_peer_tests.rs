use super::*;
use crate::cluster::membership::{Change, DecisionKind, Membership, SetupRequest};
use crate::cluster::peer_auth::{random_id, sign_request, HelloRequest, PeerScope};
use crate::cluster::peer_call::routes;
use axum::{body::Body, routing::post, Router};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt;

/// 清理历史 W19: the path the coordinator signs is the path this server
/// mounts. A typo in either would leave the peer clear unauthenticated, so the
/// two spellings are pinned together here.
#[test]
fn the_cluster_clean_route_survives_the_api_mount() {
    use crate::cluster::peer_call::{peer_route, RouteClass};
    let mounted = routes::HISTORY_CLEAN
        .path
        .strip_prefix("/api")
        .expect("peer routes carry the /api prefix");
    assert_eq!(mounted, "/cluster/v1/history/clean");
    assert!(matches!(
        peer_route("POST", &logical_path(mounted)),
        Some((_, RouteClass::PeerOnly))
    ));
}

async fn installation() -> (std::path::PathBuf, Arc<AuthState>) {
    let root = std::env::temp_dir().join(format!(
        "bilistream-peer-admission-{}",
        random_id().unwrap()
    ));
    let store = crate::storage::Store::open(root.join("data"), root.join("key")).unwrap();
    store
        .write(
            "config.json",
            serde_json::json!({"cluster":{"enabled":false}}),
        )
        .unwrap();
    Membership::new(Arc::clone(&store))
        .setup(
            SetupRequest {
                operation_id: random_id().unwrap(),
                expected_local_revision: 0,
                node_id: "local".into(),
                name: "local".into(),
                api_url: "https://local.example.test".into(),
                priority: 1,
            },
            true,
            || async { Ok(()) },
        )
        .await
        .unwrap();
    let auth = AuthState::open(store, Some("synthetic-password".into())).unwrap();
    (root, auth)
}

fn signed_request(
    auth: &AuthState,
    route: crate::cluster::peer_auth::RoutePolicy,
    revision: u64,
    scope: PeerScope,
    body: Vec<u8>,
) -> Request {
    let identity = auth.runtime.membership.identity().unwrap();
    let challenge = HelloRequest::new().unwrap();
    let hello = auth
        .peer_hello(&identity, &challenge)
        .unwrap()
        .verify(identity.public(), &challenge)
        .unwrap();
    let manifest = auth.runtime.membership.manifest().unwrap().unwrap();
    let signed = sign_request(
        &identity,
        &hello,
        &manifest.cluster_id,
        revision,
        scope,
        route,
        &body,
    )
    .unwrap();
    Request::builder()
        .method(route.method)
        .uri(route.path)
        .header(header::AUTHORIZATION, signed.authorization)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn cancelled_peer_mutation_finishes_before_membership_revokes_its_revision() {
    let (root, auth) = installation().await;
    let membership = auth.runtime.membership.clone();
    let identity = membership.identity().unwrap();
    let member_id = identity.public().member_id.clone();
    let operation_id = random_id().unwrap();
    membership
        .begin(
            operation_id.clone(),
            1,
            member_id.clone(),
            Change::UpdateNode {
                target_member_id: member_id.clone(),
                name: "updated".into(),
                api_url: "https://local.example.test".into(),
                priority: 1,
            },
        )
        .unwrap();
    let proposal = membership.propose(operation_id.clone(), None).unwrap();
    let prepared = membership
        .prepare(proposal, || async { Ok(()) })
        .await
        .unwrap();
    membership
        .record_prepare(operation_id.clone(), prepared)
        .unwrap();
    let decision = membership
        .decide(operation_id, DecisionKind::Commit)
        .unwrap();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route(
            routes::APPLY_NODE_MODE.path,
            post({
                let (started, release, committed, membership) = (
                    Arc::clone(&started),
                    Arc::clone(&release),
                    Arc::clone(&committed),
                    membership.clone(),
                );
                move || {
                    let (started, release, committed, membership) = (
                        Arc::clone(&started),
                        Arc::clone(&release),
                        Arc::clone(&committed),
                        membership.clone(),
                    );
                    async move {
                        // Node-mode writes deliberately survive their awaiting
                        // HTTP future; the middleware must keep admission alive.
                        tokio::spawn(async move {
                            started.notify_one();
                            release.notified().await;
                            assert_eq!(membership.manifest().unwrap().unwrap().revision, 1);
                            membership
                                .store()
                                .write("admitted-write", serde_json::json!(true))
                                .unwrap();
                            committed.store(true, Ordering::SeqCst);
                        })
                        .await
                        .unwrap();
                        StatusCode::OK
                    }
                }
            }),
        )
        .layer(axum::middleware::from_fn(require_webui_auth))
        .layer(Extension(Arc::clone(&auth)));
    let request = signed_request(
        &auth,
        routes::APPLY_NODE_MODE,
        1,
        PeerScope::Ordinary,
        b"{}".to_vec(),
    );
    let client = tokio::spawn(app.clone().oneshot(request));
    started.notified().await;
    client.abort();
    assert!(client.await.unwrap_err().is_cancelled());

    let runtime = auth.runtime.clone();
    let mut install = tokio::spawn(async move {
        runtime
            .handle_decision(&member_id, decision, DecisionKind::Commit)
            .await
            .unwrap();
    });
    // A cancelled caller cannot allow installation while its admitted write
    // is still queued. The fixture's notification keeps that write pending.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut install)
            .await
            .is_err()
    );
    assert_eq!(membership.manifest().unwrap().unwrap().revision, 1);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), install)
        .await
        .unwrap()
        .unwrap();
    assert!(committed.load(Ordering::SeqCst));
    assert_eq!(membership.manifest().unwrap().unwrap().revision, 2);
    let stale = signed_request(
        &auth,
        routes::APPLY_NODE_MODE,
        1,
        PeerScope::Ordinary,
        b"{}".to_vec(),
    );
    assert_eq!(
        app.clone().oneshot(stale).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    drop((app, auth, membership, identity));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn operation_handler_can_take_membership_write_gate() {
    let (root, auth) = installation().await;
    let membership = &auth.runtime.membership;
    let identity = membership.identity().unwrap();
    let operation_id = random_id().unwrap();
    membership
        .begin(
            operation_id.clone(),
            1,
            identity.public().member_id.clone(),
            Change::SetPublicNode {
                public_member_id: None,
            },
        )
        .unwrap();
    let proposal = membership.propose(operation_id.clone(), None).unwrap();
    let app = Router::new()
        .route(
            routes::PREPARE.path,
            post(|| async {
                let _gate = crate::cluster::membership::MEMBERSHIP_GATE.write().await;
                StatusCode::OK
            }),
        )
        .layer(axum::middleware::from_fn(require_webui_auth))
        .layer(Extension(Arc::clone(&auth)));
    let request = signed_request(
        &auth,
        routes::PREPARE,
        1,
        PeerScope::Operation(operation_id),
        serde_json::to_vec(&proposal).unwrap(),
    );
    let response = tokio::time::timeout(Duration::from_secs(5), app.oneshot(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop((auth, identity));
    std::fs::remove_dir_all(root).unwrap();
}

/// N3 (rev 4): a member signing at an older membership revision is refused
/// on both openid peer routes before the handler runs; the current
/// revision is admitted.
#[tokio::test]
async fn openid_peer_routes_refuse_a_member_signing_at_an_older_revision() {
    let (root, auth) = installation().await;
    let reached = Arc::new(AtomicBool::new(false));
    let handler = {
        let reached = Arc::clone(&reached);
        move || {
            let reached = Arc::clone(&reached);
            async move {
                reached.store(true, Ordering::SeqCst);
                StatusCode::OK
            }
        }
    };
    assert_eq!(
        routes::NOTIFY_OPENID_PUSH.path,
        routes::NOTIFY_OPENID_PULL.path
    );
    let app = Router::new()
        .route(
            routes::NOTIFY_OPENID_PUSH.path,
            post(handler.clone()).get(handler),
        )
        .layer(axum::middleware::from_fn(require_webui_auth))
        .layer(Extension(Arc::clone(&auth)));
    let current = auth
        .runtime
        .membership
        .manifest()
        .unwrap()
        .unwrap()
        .revision;
    assert!(current >= 1);
    for (route, body) in [
        (routes::NOTIFY_OPENID_PUSH, b"{}".to_vec()),
        (routes::NOTIFY_OPENID_PULL, Vec::new()),
    ] {
        let stale = signed_request(&auth, route, current - 1, PeerScope::Ordinary, body.clone());
        let status = app.clone().oneshot(stale).await.unwrap().status();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{} stale", route.method);
        assert!(
            !reached.load(Ordering::SeqCst),
            "{} handler not run",
            route.method
        );
        let fresh = signed_request(&auth, route, current, PeerScope::Ordinary, body);
        let status = app.clone().oneshot(fresh).await.unwrap().status();
        assert_eq!(status, StatusCode::OK, "{} current", route.method);
        assert!(reached.swap(false, Ordering::SeqCst));
    }
    drop((app, auth));
    std::fs::remove_dir_all(root).unwrap();
}

/// 统计 design §11: the history peer route is members only. A member at an
/// older revision is 401, a signer from another installation (not a
/// member) and an unsigned caller are refused, all before the handler; the
/// current member is admitted. A malformed body is 400 in the real handler.
#[tokio::test]
async fn history_peer_route_is_members_only_and_refuses_bad_bodies() {
    let (root, auth) = installation().await;
    let (other_root, other) = installation().await;
    let reached = Arc::new(AtomicBool::new(false));
    let handler = {
        let reached = Arc::clone(&reached);
        move || {
            let reached = Arc::clone(&reached);
            async move {
                reached.store(true, Ordering::SeqCst);
                StatusCode::OK
            }
        }
    };
    let app = Router::new()
        .route(routes::HISTORY.path, post(handler))
        .layer(axum::middleware::from_fn(require_webui_auth))
        .layer(Extension(Arc::clone(&auth)));
    let current = auth
        .runtime
        .membership
        .manifest()
        .unwrap()
        .unwrap()
        .revision;
    let body = br#"{"name":"sessions","query":{"from":1,"to":2}}"#.to_vec();

    let stale = signed_request(
        &auth,
        routes::HISTORY,
        current - 1,
        PeerScope::Ordinary,
        body.clone(),
    );
    assert_eq!(
        app.clone().oneshot(stale).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(!reached.load(Ordering::SeqCst));

    let foreign = signed_request(
        &other,
        routes::HISTORY,
        current,
        PeerScope::Ordinary,
        body.clone(),
    );
    let status = app.clone().oneshot(foreign).await.unwrap().status();
    // The shared peer-auth layer cannot verify a non-member's signature, so
    // it answers 401 (the same as every other peer route), not 403.
    assert_eq!(status, StatusCode::UNAUTHORIZED, "non-member");
    assert!(
        !reached.load(Ordering::SeqCst),
        "non-member never reaches the handler"
    );

    let unsigned = Request::builder()
        .method("POST")
        .uri(routes::HISTORY.path)
        .body(Body::from(body.clone()))
        .unwrap();
    let status = app.clone().oneshot(unsigned).await.unwrap().status();
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "{status}"
    );
    assert!(!reached.load(Ordering::SeqCst));

    let fresh = signed_request(&auth, routes::HISTORY, current, PeerScope::Ordinary, body);
    assert_eq!(
        app.clone().oneshot(fresh).await.unwrap().status(),
        StatusCode::OK
    );
    assert!(reached.swap(false, Ordering::SeqCst));

    // The real handler: a body that is not a history query is a 400.
    let real = Router::new().route(
        routes::HISTORY.path,
        post(crate::webui::api::history::peer_history),
    );
    for bad in [
        &b"not json"[..],
        br#"{"name":"sessions","query":{"from":1,"to":2},"sql":"DELETE FROM sessions"}"#,
        br#"{"name":"sessions"}"#,
    ] {
        let req = Request::builder()
            .method("POST")
            .uri(routes::HISTORY.path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bad.to_vec()))
            .unwrap();
        assert_eq!(
            real.clone().oneshot(req).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    drop((app, auth, other));
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(other_root).unwrap();
}
