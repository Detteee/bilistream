use super::*;
use crate::cluster::membership::{Membership, HOLD_STORE};
use crate::cluster::peer_auth::{
    random_id, set_response_fault, sign_request, verify_response, PeerEndpoint, PeerScope,
    RecognitionAnswer, RecognitionProbe, RecognitionVerdict, ResponseFault, AUTH_SCHEME,
};
use crate::cluster::peer_call::{peer_client, send_to_member};
use crate::storage::Store;
use serde_json::{json, Value};
use std::time::Duration;

struct Node {
    dir: std::path::PathBuf,
    store: Arc<Store>,
    auth: Arc<AuthState>,
    addr: SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

impl Node {
    async fn new(name: &str, password: Option<&str>) -> Self {
        crate::install_crypto_provider();
        let dir =
            std::env::temp_dir().join(format!("bilistream-cm-{name}-{}", random_id().unwrap()));
        let store = Store::open(dir.join("data"), dir.join("key"), None).unwrap();
        store.write("config.json", json!({})).unwrap();
        let auth = AuthState::open(store.clone(), password.map(str::to_owned)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve(listener, auth.clone(), None);
        Self {
            dir,
            store,
            auth,
            addr,
            server,
        }
    }
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    fn membership(&self) -> Membership {
        Membership::new(self.store.clone())
    }
    fn member_id(&self) -> String {
        self.membership()
            .identity()
            .unwrap()
            .public()
            .member_id
            .clone()
    }
    /// A new listener and runtime over the same committed Store: the
    /// in-memory boot nonce, replay cache and driver registry all start over.
    async fn restart(&mut self, password: &str) {
        self.server.abort();
        let _ = (&mut self.server).await;
        self.auth = AuthState::open(self.store.clone(), Some(password.into())).unwrap();
        let listener = tokio::net::TcpListener::bind(self.addr).await.unwrap();
        self.server = serve(listener, self.auth.clone(), None);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn serve(
    listener: tokio::net::TcpListener,
    auth: Arc<AuthState>,
    prefix: Option<&str>,
) -> tokio::task::JoinHandle<()> {
    let app = crate::webui::server::build_app(crate::AppState::new(), auth);
    let app = match prefix {
        Some(prefix) => axum::Router::new().nest(prefix, app),
        None => app,
    };
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    })
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .no_gzip()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn login(node: &Node, password: &str) -> String {
    let response = http()
        .post(format!("{}/api/login", node.url()))
        .json(&json!({ "password": password }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    cookie.split(';').next().unwrap().to_owned()
}

async fn browser(
    node: &Node,
    cookie: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let url = format!("{}{path}", node.url());
    let request = match method {
        "GET" => http().get(url),
        _ => http().post(url).json(&body.unwrap_or(json!({}))),
    };
    let response = request
        .header("cookie", cookie)
        .header("origin", node.url())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn setup(node: &Node, cookie: &str, create: bool, node_id: &str, priority: i32) {
    setup_at(node, cookie, create, node_id, priority, 0).await;
}

async fn setup_at(
    node: &Node,
    cookie: &str,
    create: bool,
    node_id: &str,
    priority: i32,
    revision: u64,
) {
    let (status, body) = browser(
        node,
        cookie,
        "POST",
        if create {
            "/api/cluster/create"
        } else {
            "/api/cluster/prepare-join"
        },
        Some(json!({
            "operation_id": random_id().unwrap(),
            "expected_local_revision": revision,
            "node_id": node_id,
            "name": node_id.to_uppercase(),
            "api_url": node.url(),
            "priority": priority,
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

async fn wait_completed(node: &Node, cookie: &str, id: &str) -> Value {
    for _ in 0..120 {
        let (status, body) = browser(
            node,
            cookie,
            "GET",
            &format!("/api/cluster/membership/operations/{id}"),
            None,
        )
        .await;
        if status == 200 && body["data"]["phase"] == "completed" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("operation {id} did not complete");
}

async fn add(
    coordinator: &Node,
    cookie: &str,
    target: &Node,
    password: &str,
    revision: u64,
) -> (u16, Value, String) {
    let id = random_id().unwrap();
    let (status, body) = browser(
        coordinator,
        cookie,
        "POST",
        "/api/cluster/membership/operations",
        Some(json!({
            "operation_id": id,
            "expected_revision": revision,
            "kind": "add",
            "target_url": target.url(),
            "target_password": password,
        })),
    )
    .await;
    (status, body, id)
}

/// FINISH delivery is asynchronous; the next operation needs every hold gone.
async fn wait_released(nodes: &[&Node]) {
    for _ in 0..80 {
        if nodes
            .iter()
            .all(|n| n.membership().local().unwrap().hold.is_none())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("membership hold was not released");
}

fn manifest(node: &Node) -> crate::cluster::membership::Manifest {
    node.membership().manifest().unwrap().unwrap()
}

async fn signed(
    from: &Node,
    to: &crate::cluster::membership::Descriptor,
    route: crate::cluster::peer_auth::RoutePolicy,
    revision: u64,
    scope: PeerScope,
    body: Vec<u8>,
) -> std::io::Result<crate::cluster::peer_auth::PeerResponse> {
    let cluster_id = manifest(from).cluster_id;
    send_to_member(
        &from.membership(),
        &peer_client().unwrap(),
        to,
        &cluster_id,
        revision,
        scope,
        route,
        body,
        Duration::from_secs(5),
    )
    .await
}

#[tokio::test]
async fn three_panels_enroll_and_offline_removal_revokes_every_peer_route() {
    let a = Node::new("a", Some("pw-a")).await;
    let b = Node::new("b", Some("pw-b")).await;
    let mut c = Node::new("c", Some("pw-c")).await;
    let (ca, cb, cc) = (
        login(&a, "pw-a").await,
        login(&b, "pw-b").await,
        login(&c, "pw-c").await,
    );
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    setup(&c, &cc, false, "c", 5).await;

    // Wrong target password: refused without echo, nothing reserved.
    let (status, body, _) = add(&a, &ca, &b, "not-the-password", 1).await;
    assert_eq!(status, 403);
    assert!(!body.to_string().contains("not-the-password"));
    assert_eq!(
        b.membership().local().unwrap().lifecycle,
        Lifecycle::JoinReady
    );

    let (status, body, id) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.to_string().contains("pw-b"));
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;
    // Any enrolled panel may add the next member.
    let (stale, _, _) = add(&b, &cb, &c, "pw-c", 1).await;
    assert_eq!(stale, 409);
    let (status, body, id) = add(&b, &cb, &c, "pw-c", 2).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&b, &cb, &id).await;
    wait_released(&[&a, &b, &c]).await;
    let digest = manifest(&a).digest;
    assert_eq!(manifest(&a).revision, 3);
    for node in [&b, &c] {
        assert_eq!(manifest(node).digest, digest);
        assert_eq!(
            node.membership().local().unwrap().lifecycle,
            Lifecycle::Managed
        );
    }
    let (status, view) = browser(&c, &cc, "GET", "/api/cluster/membership", None).await;
    assert_eq!(status, 200);
    assert_eq!(view["data"]["members"].as_array().unwrap().len(), 3);
    assert!(!view.to_string().contains("private_key"));

    // Changing a recorded operation ID's intent is a conflict.
    let (status, _) = browser(
        &b,
        &cb,
        "POST",
        "/api/cluster/membership/operations",
        Some(json!({
        "operation_id": id, "expected_revision": 2, "kind": "add",
        "target_url": a.url(), "target_password": "x"})),
    )
    .await;
    assert_eq!(status, 409);

    let a_desc = manifest(&a).member(&a.member_id()).unwrap().clone();
    let ok = signed(
        &c,
        &a_desc,
        routes::CAPABILITIES,
        3,
        PeerScope::Ordinary,
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(ok.status, 200);

    // Remove the offline standby with two of three members online.
    c.server.abort();
    let c_id = c.member_id();
    let remove = random_id().unwrap();
    let (status, body) = browser(&a, &ca, "POST", "/api/cluster/membership/operations", Some(json!({
        "operation_id": remove, "expected_revision": 3, "kind": "remove", "target_member_id": c_id}))).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &remove).await;
    for node in [&a, &b] {
        let m = manifest(node);
        assert_eq!(m.revision, 4);
        assert!(m.member(&c_id).is_none());
    }

    // The removed node returns believing it is still a member. Every
    // peer-authorized route at every retained node rejects its identity.
    c.restart("pw-c").await;
    let op = a.membership().operation(&remove).unwrap().unwrap();
    for target in [&a, &b] {
        let desc = manifest(target)
            .member(&target.member_id())
            .unwrap()
            .clone();
        for route in [
            routes::HEARTBEAT,
            routes::SELF_CHECK,
            routes::CAPABILITIES,
            routes::EXPORT_CONFIG,
            routes::YT_INDEX,
            routes::APPLY_NODE_MODE,
            routes::SYNC_CONFIG,
            routes::CACHE_MONITOR_STATE,
            routes::APPLY_PUBLIC_STATUS,
            routes::SETTINGS,
            routes::FORWARD,
            routes::DRAIN,
            routes::AUTO_FAILOVER,
            routes::FAILOVER,
            routes::RESTART,
        ] {
            let body = if route.method == "GET" {
                vec![]
            } else {
                b"{}".to_vec()
            };
            for revision in [3, 4] {
                let error = signed(
                    &c,
                    &desc,
                    route,
                    revision,
                    PeerScope::Ordinary,
                    body.clone(),
                )
                .await
                .err()
                .unwrap();
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "{}",
                    route.path
                );
            }
        }
        let status = serde_json::to_vec(&StatusRequest {
            intent: op.intent.clone(),
            retry: false,
        })
        .unwrap();
        let error = signed(
            &c,
            &desc,
            routes::STATUS,
            3,
            PeerScope::Operation(remove.clone()),
            status,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
    let b_desc = manifest(&b).member(&b.member_id()).unwrap().clone();
    assert_eq!(
        signed(
            &a,
            &b_desc,
            routes::CAPABILITIES,
            4,
            PeerScope::Ordinary,
            vec![]
        )
        .await
        .unwrap()
        .status,
        200
    );

    // Self-removal from the departing panel is coordinated by a retained
    // member; the departing node persists monitors off and enters left.
    wait_released(&[&a, &b]).await;
    let leave = random_id().unwrap();
    let (status, body) = browser(&b, &cb, "POST", "/api/cluster/membership/operations", Some(json!({
        "operation_id": leave, "expected_revision": 4, "kind": "remove", "target_member_id": b.member_id()}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["coordinator_node_id"], "a");
    wait_completed(&a, &ca, &leave).await;
    for _ in 0..80 {
        if b.membership().local().unwrap().lifecycle == Lifecycle::Left {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(b.membership().local().unwrap().lifecycle, Lifecycle::Left);
    assert!(crate::cluster::membership::hold_reason(&b.store).is_some());
    let config = b.store.read("config.json").unwrap().unwrap().value;
    assert_eq!(config["cluster"]["enabled"], false);
    assert_eq!(manifest(&a).members.len(), 1);
}

#[tokio::test]
async fn coordinator_restart_resumes_before_and_after_its_decision() {
    let mut a = Node::new("ra", Some("pw-a")).await;
    let b = Node::new("rb", Some("pw-b")).await;
    let (ca, cb) = (login(&a, "pw-a").await, login(&b, "pw-b").await);
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    let (status, body, id) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;
    let (me, b_id) = (a.member_id(), b.member_id());
    let update = |priority| Change::UpdateNode {
        target_member_id: b_id.clone(),
        name: "B".into(),
        api_url: b.url(),
        priority,
    };

    // Preparing journaled, nothing sent, then the coordinator restarts.
    let first = random_id().unwrap();
    a.membership()
        .begin(first.clone(), 2, me.clone(), update(3))
        .unwrap();
    a.membership().propose(first.clone(), None).unwrap();
    a.restart("pw-a").await;
    a.auth.runtime().recover().await.unwrap();
    let ca = login(&a, "pw-a").await;
    wait_completed(&a, &ca, &first).await;
    wait_released(&[&a, &b]).await;
    assert_eq!(manifest(&b).member(&b_id).unwrap().priority, 3);

    // Both prepares and COMMIT durable before the restart.
    let second = random_id().unwrap();
    a.membership()
        .begin(second.clone(), 3, me.clone(), update(4))
        .unwrap();
    let proposal = a.membership().propose(second.clone(), None).unwrap();
    let remote = b
        .auth
        .runtime()
        .handle_prepare(&me, proposal.clone())
        .await
        .unwrap();
    let local = a
        .membership()
        .prepare(proposal, || async { Ok(()) })
        .await
        .unwrap();
    a.membership()
        .record_prepare(second.clone(), remote)
        .unwrap();
    a.membership()
        .record_prepare(second.clone(), local)
        .unwrap();
    a.membership()
        .decide(second.clone(), DecisionKind::Commit)
        .unwrap();
    assert!(b.membership().local().unwrap().hold.is_some());
    a.restart("pw-a").await;
    a.auth.runtime().recover().await.unwrap();
    let ca = login(&a, "pw-a").await;
    wait_completed(&a, &ca, &second).await;
    wait_released(&[&a, &b]).await;
    assert_eq!(manifest(&b).digest, manifest(&a).digest);
    assert_eq!(manifest(&b).member(&b_id).unwrap().priority, 4);
    assert!(b.membership().local().unwrap().hold.is_none());

    // FINISH is already durable, then the coordinator restarts before delivery.
    let third = random_id().unwrap();
    a.membership()
        .begin(third.clone(), 4, me.clone(), update(5))
        .unwrap();
    let proposal = a.membership().propose(third.clone(), None).unwrap();
    let remote = b
        .auth
        .runtime()
        .handle_prepare(&me, proposal.clone())
        .await
        .unwrap();
    let local = a
        .membership()
        .prepare(proposal, || async { Ok(()) })
        .await
        .unwrap();
    a.membership()
        .record_prepare(third.clone(), remote)
        .unwrap();
    a.membership().record_prepare(third.clone(), local).unwrap();
    let decision = a
        .membership()
        .decide(third.clone(), DecisionKind::Commit)
        .unwrap();
    let local_installed = a
        .auth
        .runtime()
        .handle_decision(&me, decision.clone(), DecisionKind::Commit)
        .await
        .unwrap()
        .unwrap();
    let remote_installed = b
        .auth
        .runtime()
        .handle_decision(&me, decision, DecisionKind::Commit)
        .await
        .unwrap()
        .unwrap();
    a.membership()
        .record_installed(third.clone(), local_installed)
        .unwrap();
    a.membership()
        .record_installed(third.clone(), remote_installed)
        .unwrap();
    a.membership().decide_finish(third.clone()).unwrap();
    assert!(b.membership().local().unwrap().hold.is_some());
    a.restart("pw-a").await;
    a.auth.runtime().recover().await.unwrap();
    let ca = login(&a, "pw-a").await;
    wait_completed(&a, &ca, &third).await;
    wait_released(&[&a, &b]).await;
    assert_eq!(manifest(&b).member(&b_id).unwrap().priority, 5);
    assert!(b.membership().local().unwrap().hold.is_none());
}

#[tokio::test]
async fn signed_routes_keep_exact_bytes_through_prefix_and_compression() {
    let a = Node::new("sa", Some("pw-a")).await;
    let cookie = login(&a, "pw-a").await;
    setup(&a, &cookie, true, "a", 1).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prefixed = format!("http://{}/prefix", listener.local_addr().unwrap());
    let _prefix = serve(listener, a.auth.clone(), Some("/prefix"));
    let m = manifest(&a);
    let mut me = m.member(&a.member_id()).unwrap().clone();
    me.api_url = prefixed.clone();
    let reply = signed(
        &a,
        &me,
        routes::CAPABILITIES,
        m.revision,
        PeerScope::Ordinary,
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&reply.body).unwrap()["config_sync"],
        2
    );

    // A client asking for gzip still receives the exact signed bytes.
    let identity = a.membership().identity().unwrap();
    let endpoint = PeerEndpoint::parse(&prefixed, true).unwrap();
    let hello = peer_client()
        .unwrap()
        .hello(&endpoint, identity.public())
        .await
        .unwrap();
    let request = sign_request(
        &identity,
        &hello,
        &m.cluster_id,
        m.revision,
        PeerScope::Ordinary,
        routes::CAPABILITIES,
        b"",
    )
    .unwrap();
    let response = http()
        .get(endpoint.route_url(routes::CAPABILITIES.path).unwrap())
        .header("authorization", request.authorization.clone())
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert!(response.headers().get("content-encoding").is_none());
    let (status, headers) = (response.status().as_u16(), response.headers().clone());
    let content_type = headers["content-type"].to_str().unwrap().to_owned();
    let body = response.bytes().await.unwrap();
    verify_response(
        &request,
        identity.public(),
        &headers,
        status,
        &content_type,
        &body,
        routes::CAPABILITIES,
    )
    .unwrap();
    // The same layer does compress ordinary responses.
    let browser = http()
        .get(format!("{}/api/auth", a.url()))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(browser.headers()["content-encoding"], "gzip");

    // A replayed request and a query-string variant are both refused.
    let replay = http()
        .get(endpoint.route_url(routes::CAPABILITIES.path).unwrap())
        .header("authorization", request.authorization.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 401);
    let fresh = sign_request(
        &identity,
        &hello,
        &m.cluster_id,
        m.revision,
        PeerScope::Ordinary,
        routes::CAPABILITIES,
        b"",
    )
    .unwrap();
    let query = http()
        .get(format!(
            "{}?x=1",
            endpoint.route_url(routes::CAPABILITIES.path).unwrap()
        ))
        .header("authorization", fresh.authorization)
        .send()
        .await
        .unwrap();
    assert_eq!(query.status(), 401);
}

#[tokio::test]
async fn invalid_or_node_authorization_never_reaches_browser_authority() {
    // Passwordless loopback: browser routes are open without credentials.
    let open = Node::new("open", None).await;
    let url = format!("{}/api/cluster/membership", open.url());
    assert_eq!(http().get(&url).send().await.unwrap().status(), 200);
    for value in [
        format!("{AUTH_SCHEME}garbage"),
        "Bearer synthetic-cluster-key-0123456789abcdef".into(),
    ] {
        let response = http()
            .get(&url)
            .header("authorization", value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    // Peer-only paths are closed to browsers even without a password.
    for path in [
        "/api/cluster/heartbeat",
        "/api/cluster/v1/operation/prepare",
        "/api/cluster/v1/settings",
        "/api/cluster/v1/membership/recognition",
        "/api/cluster/sync-membership",
    ] {
        let response = http()
            .post(format!("{}{path}", open.url()))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert!(
            matches!(response.status().as_u16(), 401 | 404 | 405),
            "{path} {}",
            response.status()
        );
    }
    let hello = http()
        .post(format!("{}/api/cluster/v1/hello", open.url()))
        .header("authorization", format!("{AUTH_SCHEME}x"))
        .json(&json!({"challenge":"x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(hello.status(), 401);

    // A valid member signature cannot call a browser-only route.
    let a = Node::new("na", Some("pw-a")).await;
    let cookie = login(&a, "pw-a").await;
    setup(&a, &cookie, true, "a", 1).await;
    let m = manifest(&a);
    let identity = a.membership().identity().unwrap();
    let endpoint = PeerEndpoint::parse(&a.url(), true).unwrap();
    let hello = peer_client()
        .unwrap()
        .hello(&endpoint, identity.public())
        .await
        .unwrap();
    for path in ["/api/cluster/membership", "/api/cluster/create"] {
        let route = crate::cluster::peer_auth::RoutePolicy {
            method: "POST",
            path,
            max_request_bytes: 1024,
            max_response_bytes: 4096,
        };
        let request = sign_request(
            &identity,
            &hello,
            &m.cluster_id,
            m.revision,
            PeerScope::Ordinary,
            route,
            b"{}",
        )
        .unwrap();
        let response = http()
            .post(format!("{}{path}", a.url()))
            .header("authorization", request.authorization)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{path}");
    }
    // Browser cookies never satisfy a peer-only route.
    let (status, _) = browser(
        &a,
        &cookie,
        "POST",
        "/api/cluster/heartbeat",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 401);
    // Byte caps apply before authentication or parsing.
    let response = http()
        .post(format!("{}/api/cluster/self-check", a.url()))
        .header("authorization", format!("{AUTH_SCHEME}x"))
        .body(vec![b'x'; 4096])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    // Membership mutations need the panel's own origin.
    let response = http()
        .post(format!("{}/api/cluster/membership/operations", a.url()))
        .header("cookie", &cookie)
        .header("origin", "http://evil.test")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    // Create/join require a configured panel password.
    let (status, body) = browser(
        &open,
        "",
        "POST",
        "/api/cluster/create",
        Some(json!({
        "operation_id": random_id().unwrap(), "expected_local_revision": 0, "node_id": "o",
        "name": "O", "api_url": open.url(), "priority": 0})),
    )
    .await;
    assert_eq!(status, 409, "{body}");
}

#[tokio::test]
async fn durable_join_hold_blocks_monitoring_before_the_cluster_enabled_shortcut() {
    let node = Node::new("hold", Some("pw")).await;
    let cookie = login(&node, "pw").await;
    let mut cfg = crate::cluster::tests::test_config("hold", 0);
    cfg.cluster.enabled = false;
    let reason = |store: Arc<Store>, cfg: crate::config::Config| {
        HOLD_STORE.scope(store, async move {
            crate::cluster::local_monitoring_allowed(&cfg)
        })
    };
    assert!(reason(node.store.clone(), cfg.clone()).await);
    setup(&node, &cookie, false, "hold", 0).await;
    assert!(!reason(node.store.clone(), cfg.clone()).await);
    // A legacy enabled topology without an identity stays held after startup.
    let legacy = Node::new("legacy", Some("pw")).await;
    legacy
        .store
        .write("config.json", json!({"cluster": {"enabled": true}}))
        .unwrap();
    legacy.membership().initialize().unwrap();
    assert_eq!(
        legacy.membership().local().unwrap().lifecycle,
        Lifecycle::IncompatibleHeld
    );
    assert!(!reason(legacy.store.clone(), cfg).await);
}

#[tokio::test]
async fn departing_node_restarts_between_decision_and_finish() {
    let a = Node::new("da", Some("pw-a")).await;
    let mut b = Node::new("db", Some("pw-b")).await;
    let (ca, cb) = (login(&a, "pw-a").await, login(&b, "pw-b").await);
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    let (status, body, id) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;

    let (me, b_id) = (a.member_id(), b.member_id());
    let remove = random_id().unwrap();
    let change = Change::Remove {
        target_member_id: b_id,
        replacement_public_member_id: None,
    };
    a.membership()
        .begin(remove.clone(), 2, me.clone(), change)
        .unwrap();
    let proposal = a.membership().propose(remove.clone(), None).unwrap();
    let remote = b
        .auth
        .runtime()
        .handle_prepare(&me, proposal.clone())
        .await
        .unwrap();
    let local = a
        .membership()
        .prepare(proposal, || async { Ok(()) })
        .await
        .unwrap();
    a.membership()
        .record_prepare(remove.clone(), remote)
        .unwrap();
    a.membership()
        .record_prepare(remove.clone(), local)
        .unwrap();
    let decision = a
        .membership()
        .decide(remove.clone(), DecisionKind::Commit)
        .unwrap();
    b.auth
        .runtime()
        .handle_decision(&me, decision, DecisionKind::Commit)
        .await
        .unwrap();
    // Installed without its own membership, still held, awaiting FINISH.
    b.membership().initialize().unwrap();
    assert!(crate::cluster::membership::hold_reason(&b.store).is_some());
    b.restart("pw-b").await;
    a.auth.runtime().recover().await.unwrap();
    wait_completed(&a, &ca, &remove).await;
    for _ in 0..80 {
        if b.membership().local().unwrap().lifecycle == Lifecycle::Left {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(b.membership().local().unwrap().lifecycle, Lifecycle::Left);
    b.membership().initialize().unwrap();
}

#[tokio::test]
async fn each_panel_can_edit_and_a_left_node_reenrolls_under_the_same_node_id() {
    let a = Node::new("ea", Some("pw-a")).await;
    let b = Node::new("eb", Some("pw-b")).await;
    let c = Node::new("ec", Some("pw-c")).await;
    let (ca, cb, cc) = (
        login(&a, "pw-a").await,
        login(&b, "pw-b").await,
        login(&c, "pw-c").await,
    );
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    setup(&c, &cc, false, "c", 5).await;

    let (status, body, first) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &first).await;
    wait_released(&[&a, &b]).await;
    // The same operation id is a continuation, not a second enrollment.
    let (status, body) = browser(
        &a,
        &ca,
        "POST",
        "/api/cluster/membership/operations",
        Some(json!({
            "operation_id": first,
            "expected_revision": 1,
            "kind": "add",
            "target_url": b.url(),
            "target_password": "pw-b",
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.to_string().contains("pw-b"));
    assert_eq!(manifest(&a).members.len(), 2);

    let (status, body, id) = add(&b, &cb, &c, "pw-c", 2).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&b, &cb, &id).await;
    wait_released(&[&a, &b, &c]).await;

    let public = random_id().unwrap();
    let (status, body) = browser(
        &c,
        &cc,
        "POST",
        "/api/cluster/membership/operations",
        Some(json!({
            "operation_id": public,
            "expected_revision": 3,
            "kind": "set_public_node",
            "public_member_id": c.member_id(),
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&c, &cc, &public).await;
    wait_released(&[&a, &b, &c]).await;
    assert_eq!(
        manifest(&a).public_member_id.as_deref(),
        Some(c.member_id().as_str())
    );
    for node in [&a, &b, &c] {
        let config = node.store.read("config.json").unwrap().unwrap().value;
        assert_eq!(config["cluster"]["public_status"]["node_id"], "c");
        assert_eq!(manifest(node).digest, manifest(&a).digest);
    }

    let old_b = b.member_id();
    let leave = random_id().unwrap();
    let (status, body) = browser(
        &b,
        &cb,
        "POST",
        "/api/cluster/membership/operations",
        Some(json!({
            "operation_id": leave,
            "expected_revision": 4,
            "kind": "remove",
            "target_member_id": old_b,
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &leave).await;
    for _ in 0..80 {
        if b.membership().local().unwrap().lifecycle == Lifecycle::Left {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(b.membership().local().unwrap().lifecycle, Lifecycle::Left);
    assert_eq!(b.member_id(), old_b);
    let a_desc = manifest(&a).member(&a.member_id()).unwrap().clone();
    let rejected = signed(
        &b,
        &a_desc,
        routes::CAPABILITIES,
        manifest(&a).revision,
        PeerScope::Ordinary,
        vec![],
    )
    .await
    .err()
    .unwrap();
    assert_eq!(rejected.kind(), std::io::ErrorKind::PermissionDenied);

    let (status, view) = browser(&b, &cb, "GET", "/api/cluster/membership", None).await;
    assert_eq!(status, 200, "{view}");
    let (status, body) = browser(
        &b,
        &cb,
        "POST",
        "/api/cluster/prepare-join",
        Some(json!({
            "operation_id": random_id().unwrap(),
            "expected_local_revision": view["data"]["local_revision"],
            "node_id": "b",
            "name": "B",
            "api_url": b.url(),
            "priority": 7,
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let rejoined = b.member_id();
    assert_ne!(rejoined, old_b);
    let revision = manifest(&a).revision;
    let (status, body, id) = add(&a, &ca, &b, "pw-b", revision).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;
    assert!(manifest(&a).member(&old_b).is_none());
    assert_eq!(manifest(&a).member(&rejoined).unwrap().node_id, "b");
    let desc = manifest(&a).member(&a.member_id()).unwrap().clone();
    assert_eq!(
        signed(
            &b,
            &desc,
            routes::CAPABILITIES,
            manifest(&a).revision,
            PeerScope::Ordinary,
            vec![],
        )
        .await
        .unwrap()
        .status,
        200
    );
}

async fn enrolled(tag: &str) -> (Node, Node, Node, String, String, String) {
    let a = Node::new(&format!("{tag}-a"), Some("pw-a")).await;
    let b = Node::new(&format!("{tag}-b"), Some("pw-b")).await;
    let c = Node::new(&format!("{tag}-c"), Some("pw-c")).await;
    let (ca, cb, cc) = (
        login(&a, "pw-a").await,
        login(&b, "pw-b").await,
        login(&c, "pw-c").await,
    );
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    setup(&c, &cc, false, "c", 5).await;
    let (status, body, id) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;
    let (status, body, id) = add(&b, &cb, &c, "pw-c", 2).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&b, &cb, &id).await;
    wait_released(&[&a, &b, &c]).await;
    (a, b, c, ca, cb, cc)
}

/// Remove `c` while it is offline, then bring it back still believing it is a member.
async fn remove_offline_standby(a: &Node, b: &Node, c: &mut Node, cookie: &str) -> String {
    let removed = c.member_id();
    c.server.abort();
    let revision = manifest(a).revision;
    let remove = random_id().unwrap();
    let (status, body) = browser(
        a,
        cookie,
        "POST",
        "/api/cluster/membership/operations",
        Some(json!({
            "operation_id": remove,
            "expected_revision": revision,
            "kind": "remove",
            "target_member_id": removed,
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    wait_completed(a, cookie, &remove).await;
    wait_released(&[a, b]).await;
    assert!(manifest(a).member(&removed).is_none());
    assert!(manifest(b).member(&removed).is_none());
    c.restart("pw-c").await;
    removed
}

async fn leave(node: &Node, cookie: &str) -> (u16, Value) {
    browser(
        node,
        cookie,
        "POST",
        "/api/cluster/membership/leave",
        Some(json!({})),
    )
    .await
}

fn durable(node: &Node) -> Value {
    json!({
        "local": node.store.read("cluster-local").unwrap().unwrap().value,
        "manifest": node.store.read("cluster-membership").unwrap().unwrap().value,
        "config": node.store.read("config.json").unwrap().unwrap().value,
        "channels": node.store.read("channels.json").unwrap().map(|doc| doc.value),
        "password": node.store.revision("webui-password").unwrap(),
        "member_id": node.store.read("cluster-identity").unwrap().unwrap().value["member_id"],
    })
}

fn enable_monitors(store: &Store) {
    let mut config = store.read("config.json").unwrap().unwrap().value;
    config["enable_youtube_monitor"] = json!(true);
    config["enable_twitch_monitor"] = json!(true);
    for (section, field) in [
        ("youtube", "enable_monitor"),
        ("twitch", "enable_monitor"),
        ("niconico", "enable_monitor"),
        ("priority_channel", "enabled"),
        ("priority_channel", "auto_restart"),
        ("bililive", "enable_danmaku_command"),
    ] {
        if !config[section].is_object() {
            config[section] = json!({});
        }
        config[section][field] = json!(true);
    }
    store.write("config.json", config).unwrap();
}

fn monitors_and_peers_cleared(config: &Value) -> bool {
    config["enable_youtube_monitor"] == false
        && config["enable_twitch_monitor"] == false
        && config["youtube"]["enable_monitor"] == false
        && config["twitch"]["enable_monitor"] == false
        && config["niconico"]["enable_monitor"] == false
        && config["priority_channel"]["enabled"] == false
        && config["priority_channel"]["auto_restart"] == false
        && config["bililive"]["enable_danmaku_command"] == false
        && config["cluster"]["enabled"] == false
        && config["cluster"]["peers"] == json!([])
}

struct FaultGuard(String);
impl Drop for FaultGuard {
    fn drop(&mut self) {
        set_response_fault(&self.0, None);
    }
}

#[tokio::test]
async fn removed_standby_leaves_only_after_every_retained_node_rejects_its_key() {
    let a = Node::new("leave-a", Some("pw-a")).await;
    let b = Node::new("leave-b", Some("pw-b")).await;
    let mut c = Node::new("leave-c", Some("pw-c")).await;
    let (ca, cb, cc) = (
        login(&a, "pw-a").await,
        login(&b, "pw-b").await,
        login(&c, "pw-c").await,
    );
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    setup(&c, &cc, false, "c", 5).await;
    let (status, body, id) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;
    let (status, body, id) = add(&a, &ca, &c, "pw-c", 2).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b, &c]).await;

    let old_c = remove_offline_standby(&a, &b, &mut c, &ca).await;
    let cc = login(&c, "pw-c").await;
    assert_eq!(
        c.membership().local().unwrap().lifecycle,
        Lifecycle::Managed
    );
    assert_eq!(c.member_id(), old_c);
    assert!(manifest(&c).member(&old_c).is_some());
    enable_monitors(&c.store);
    c.store
        .write("channels.json", json!([{"name": "示例频道 001"}]))
        .unwrap();
    let password = c.store.revision("webui-password").unwrap();
    let channels = c.store.read("channels.json").unwrap().unwrap().value;

    // A hold blocks the button before any departure.
    let mut held = c.store.read("cluster-local").unwrap().unwrap().value;
    held["hold"] = json!("maintenance");
    c.store.write("cluster-local", held).unwrap();
    let (status, body) = leave(&c, &cc).await;
    assert_eq!(status, 409, "{body}");
    assert!(body["message"]
        .as_str()
        .unwrap_or_default()
        .contains("维护"));
    assert_eq!(
        c.membership().local().unwrap().lifecycle,
        Lifecycle::Managed
    );
    assert_eq!(
        c.store.read("config.json").unwrap().unwrap().value["enable_youtube_monitor"],
        true
    );

    let mut held = c.store.read("cluster-local").unwrap().unwrap().value;
    held["hold"] = Value::Null;
    c.store.write("cluster-local", held).unwrap();

    let a_desc = manifest(&a).member(&a.member_id()).unwrap().clone();
    let stale = manifest(&c).revision;
    assert!(stale < manifest(&a).revision);
    let identity = c.membership().identity().unwrap();
    let probe = RecognitionProbe {
        cluster_id: manifest(&c).cluster_id.clone(),
        member_id: identity.public().member_id.clone(),
        public_key: identity.public().public_key.clone(),
    };
    let retained_before = a.store.revision("cluster-membership").unwrap();
    // The caller's stored revision is stale. Ordinary routes still reject it;
    // the probe must not.
    let response = signed(
        &c,
        &a_desc,
        routes::RECOGNITION,
        999,
        PeerScope::Ordinary,
        serde_json::to_vec(&probe).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    assert!(!response
        .body
        .windows(11)
        .any(|window| window == b"private_key"));
    let answer: RecognitionAnswer = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(answer.verdict, RecognitionVerdict::Absent);
    assert_eq!(answer.member_id, old_c);
    answer.verify(&a_desc.identity()).unwrap();
    let mut forged = answer.clone();
    forged.verdict = RecognitionVerdict::Present;
    assert!(forged.verify(&a_desc.identity()).is_err());
    assert_eq!(
        a.store.revision("cluster-membership").unwrap(),
        retained_before
    );
    for revision in [stale, manifest(&a).revision] {
        let error = signed(
            &c,
            &a_desc,
            routes::HEARTBEAT,
            revision,
            PeerScope::Ordinary,
            b"{}".to_vec(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    let (status, body) = leave(&c, &cc).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(c.membership().local().unwrap().lifecycle, Lifecycle::Left);
    assert_eq!(
        c.membership().local().unwrap().hold.as_deref(),
        Some("left")
    );
    assert!(crate::cluster::membership::hold_reason(&c.store).is_some());
    assert!(crate::cluster::membership::trust_snapshot(&c.store).is_err());
    let config = c.store.read("config.json").unwrap().unwrap().value;
    assert!(monitors_and_peers_cleared(&config), "{config}");
    assert_eq!(
        c.store.read("channels.json").unwrap().unwrap().value,
        channels
    );
    assert_eq!(c.store.revision("webui-password").unwrap(), password);
    assert_eq!(c.member_id(), old_c);
    assert_eq!(
        a.store.revision("cluster-membership").unwrap(),
        retained_before
    );
    let (status, view) = browser(&c, &cc, "GET", "/api/cluster/membership", None).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["data"]["lifecycle"], "left");
    assert!(view["data"]["members"].as_array().unwrap().is_empty());

    let local_revision = c
        .store
        .revision(crate::cluster::membership::LOCAL_RECORD)
        .unwrap()
        .unwrap();
    setup_at(&c, &cc, false, "c", 5, local_revision).await;
    assert_eq!(
        c.membership().local().unwrap().lifecycle,
        Lifecycle::JoinReady
    );
    let rejoined = c.member_id();
    assert_ne!(rejoined, old_c);
    let (status, body, id) = add(&a, &ca, &c, "pw-c", manifest(&a).revision).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b, &c]).await;
    assert!(manifest(&a).member(&old_c).is_none());
    assert_eq!(manifest(&a).member(&rejoined).unwrap().node_id, "c");
    assert_eq!(
        c.store.read("channels.json").unwrap().unwrap().value,
        channels
    );
    assert_eq!(c.store.revision("webui-password").unwrap(), password);
}

#[tokio::test]
async fn removed_standby_does_not_leave_when_a_retained_node_is_down() {
    let a = Node::new("down-a", Some("pw-a")).await;
    let b = Node::new("down-b", Some("pw-b")).await;
    let mut c = Node::new("down-c", Some("pw-c")).await;
    let ca = login(&a, "pw-a").await;
    let cb = login(&b, "pw-b").await;
    let cc = login(&c, "pw-c").await;
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    setup(&c, &cc, false, "c", 5).await;
    let (status, body, id) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;
    let (status, body, id) = add(&a, &ca, &c, "pw-c", 2).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b, &c]).await;
    remove_offline_standby(&a, &b, &mut c, &ca).await;
    let cc = login(&c, "pw-c").await;
    enable_monitors(&c.store);
    let before = durable(&c);
    a.server.abort();
    let (status, body) = leave(&c, &cc).await;
    assert_eq!(status, 409, "{body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains('a') && message.contains("没有确认"),
        "{message}"
    );
    assert_eq!(durable(&c), before);
    assert_eq!(
        c.membership().local().unwrap().lifecycle,
        Lifecycle::Managed
    );
}

#[tokio::test]
async fn a_node_still_in_the_other_manifests_cannot_leave() {
    let (a, b, c, _ca, _cb, cc) = enrolled("stay").await;
    enable_monitors(&c.store);
    let before = durable(&c);
    let (status, body) = leave(&c, &cc).await;
    assert_eq!(status, 409, "{body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("仍承认") && message.contains("移除"),
        "{message}"
    );
    assert_eq!(durable(&c), before);
    assert_eq!(
        c.membership().local().unwrap().lifecycle,
        Lifecycle::Managed
    );
    assert_eq!(manifest(&a).members.len(), 3);
    assert_eq!(manifest(&b).members.len(), 3);
}

#[tokio::test]
async fn tampered_or_unsigned_absence_answer_does_not_leave() {
    let a = Node::new("bad-a", Some("pw-a")).await;
    let b = Node::new("bad-b", Some("pw-b")).await;
    let mut c = Node::new("bad-c", Some("pw-c")).await;
    let ca = login(&a, "pw-a").await;
    let cb = login(&b, "pw-b").await;
    let cc = login(&c, "pw-c").await;
    setup(&a, &ca, true, "a", 10).await;
    setup(&b, &cb, false, "b", 7).await;
    setup(&c, &cc, false, "c", 5).await;
    let (status, body, id) = add(&a, &ca, &b, "pw-b", 1).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b]).await;
    let (status, body, id) = add(&a, &ca, &c, "pw-c", 2).await;
    assert_eq!(status, 200, "{body}");
    wait_completed(&a, &ca, &id).await;
    wait_released(&[&a, &b, &c]).await;
    remove_offline_standby(&a, &b, &mut c, &ca).await;
    let cc = login(&c, "pw-c").await;
    enable_monitors(&c.store);
    let before = durable(&c);
    let endpoint = a.url();
    let _guard = FaultGuard(endpoint.clone());
    for fault in [ResponseFault::Unsigned, ResponseFault::TamperBody] {
        set_response_fault(&endpoint, Some(fault));
        let (status, body) = leave(&c, &cc).await;
        assert_eq!(status, 409, "{fault:?} {body}");
        let message = body["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("没有确认") && message.contains('a'),
            "{message}"
        );
        assert!(!message.contains("仍承认"), "{message}");
        assert_eq!(durable(&c), before, "{fault:?}");
        assert_eq!(
            c.membership().local().unwrap().lifecycle,
            Lifecycle::Managed
        );
    }
}
