use super::*;
use crate::cluster::membership::{Change, DecisionKind, Membership, SetupRequest};
use crate::cluster::peer_auth::{random_id, sign_request, HelloRequest, PeerScope};
use crate::cluster::peer_call::routes;
use axum::{body::Body, routing::post, Router};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt;

async fn installation() -> (std::path::PathBuf, Arc<AuthState>) {
    let root = std::env::temp_dir().join(format!(
        "bilistream-peer-admission-{}",
        random_id().unwrap()
    ));
    let store = crate::storage::Store::open(root.join("data"), root.join("key"), None).unwrap();
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
