use super::*;
use crate::storage::Store;
use axum::http::{header, HeaderMap, HeaderValue};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Installation {
    directory: PathBuf,
    store: Arc<Store>,
}
impl Installation {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("bilistream-peer-auth-{}", random_id().unwrap()));
        let store = Store::open(directory.join("data"), directory.join("keys/key"), None).unwrap();
        Self { directory, store }
    }
    fn identity(&self) -> NodeIdentity {
        self.store.transaction(NodeIdentity::create_in).unwrap()
    }
}
impl Drop for Installation {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

const ROUTE: RoutePolicy = RoutePolicy {
    method: "POST",
    path: "/api/cluster/self-check",
    max_request_bytes: 1024,
    max_response_bytes: 4096,
};

fn observed(receiver: &PeerReceiver, identity: &NodeIdentity) -> ObservedHello {
    let challenge = HelloRequest::new().unwrap();
    receiver
        .hello(identity, &challenge)
        .unwrap()
        .verify(identity.public(), &challenge)
        .unwrap()
}

fn trust(identity: &NodeIdentity) -> TrustSnapshot {
    TrustSnapshot {
        cluster_id: "test-cluster".into(),
        revisions: vec![7],
        scope: PeerScope::Ordinary,
        members: vec![identity.public().clone()],
    }
}

fn headers(request: &SignedRequest) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, request.authorization.clone());
    headers
}

#[test]
fn private_identity_is_explicit_persistent_validated_and_rotates_only_after_leave() {
    let install = Installation::new();
    assert!(NodeIdentity::load(&install.store).unwrap().is_none());
    let identity = install.identity();
    let loaded = NodeIdentity::load(&install.store).unwrap().unwrap();
    assert_eq!(identity.public(), loaded.public());
    assert!(install
        .store
        .transaction(NodeIdentity::rotate_left_in)
        .is_err());
    install
        .store
        .write("cluster-local", json!({"lifecycle":"left"}))
        .unwrap();
    let replacement = install
        .store
        .transaction(NodeIdentity::rotate_left_in)
        .unwrap();
    assert_ne!(identity.public(), replacement.public());
    let proof = identity.sign(Domain::Proposal, b"proposal");
    assert!(verify_proof(replacement.public(), Domain::Proposal, b"proposal", &proof).is_err());
    let mut record = install.store.read(IDENTITY_RECORD).unwrap().unwrap().value;
    record["public_key"] = json!(identity.public().public_key);
    install.store.write(IDENTITY_RECORD, record).unwrap();
    assert!(NodeIdentity::load(&install.store).is_err());
    assert!(install.store.transaction(NodeIdentity::create_in).is_err());
}

#[test]
fn signatures_bind_exact_payload_signer_and_domain() {
    let install = Installation::new();
    let identity = install.identity();
    let proof = identity.sign(Domain::Prepare, b"prepared\0payload");
    verify_proof(
        identity.public(),
        Domain::Prepare,
        b"prepared\0payload",
        &proof,
    )
    .unwrap();
    assert!(verify_proof(
        identity.public(),
        Domain::Decision,
        b"prepared\0payload",
        &proof
    )
    .is_err());
    assert!(verify_proof(
        identity.public(),
        Domain::Prepare,
        b"preparedpayload",
        &proof
    )
    .is_err());
    let mut forged = identity.public().clone();
    forged.member_id.replace_range(..1, "g");
    assert!(verify_proof(&forged, Domain::Prepare, b"prepared\0payload", &proof).is_err());
    let mut a = Encoder::new();
    a.field(b"a");
    a.field(b"bc");
    let mut b = Encoder::new();
    b.field(b"ab");
    b.field(b"c");
    assert_ne!(a.finish(), b.finish());

    let policy = identity.sign(Domain::FencingPolicy, b"fencing-policy-v1");
    verify_proof(
        identity.public(),
        Domain::FencingPolicy,
        b"fencing-policy-v1",
        &policy,
    )
    .unwrap();
    for other_domain in [Domain::Prepare, Domain::Hello, Domain::Finish] {
        assert!(verify_proof(
            identity.public(),
            other_domain,
            b"fencing-policy-v1",
            &policy
        )
        .is_err());
    }
}

#[test]
fn hello_requires_pinned_identity_challenge_and_valid_signature() {
    let a = Installation::new();
    let b = Installation::new();
    let identity = a.identity();
    let stranger = b.identity();
    let receiver = PeerReceiver::new().unwrap();
    let challenge = HelloRequest::new().unwrap();
    let reply = receiver.hello(&identity, &challenge).unwrap();
    assert!(reply.clone().verify(stranger.public(), &challenge).is_err());
    assert!(reply
        .clone()
        .verify(identity.public(), &HelloRequest::new().unwrap())
        .is_err());
    let mut tampered = reply.clone();
    tampered.elapsed_ms += 1;
    assert!(tampered.verify(identity.public(), &challenge).is_err());
    reply.verify(identity.public(), &challenge).unwrap();
}

#[test]
fn request_binding_revocation_replay_restart_and_no_browser_fallback() {
    let a = Installation::new();
    let b = Installation::new();
    let sender = a.identity();
    let local = b.identity();
    let receiver = PeerReceiver::new().unwrap();
    let hello = observed(&receiver, &local);
    let body = b"{\"challenge\":\"one\"}";
    let request = sign_request(
        &sender,
        &hello,
        "test-cluster",
        7,
        PeerScope::Ordinary,
        ROUTE,
        body,
    )
    .unwrap();
    assert!(request.authorization.is_sensitive());
    let original = headers(&request);
    let auth = |h: &HeaderMap, method, path, body: &[u8], t: &TrustSnapshot| {
        receiver.authenticate(
            &AdmittedRequest {
                headers: h,
                method,
                path_and_query: path,
                route: ROUTE,
                body,
            },
            local.public(),
            t,
        )
    };
    let good = trust(&sender);
    assert!(auth(&original, "GET", ROUTE.path, body, &good).is_err());
    assert!(auth(
        &original,
        "POST",
        "/api/cluster/self-check?x=1",
        body,
        &good
    )
    .is_err());
    assert!(auth(&original, "POST", "/api/cluster/heartbeat", body, &good).is_err());
    assert!(auth(&original, "POST", ROUTE.path, b"{}", &good).is_err());
    let mut revoked = trust(&sender);
    revoked.members.clear();
    assert!(auth(&original, "POST", ROUTE.path, body, &revoked).is_err());
    let mut stale = trust(&sender);
    stale.revisions = vec![8];
    assert!(auth(&original, "POST", ROUTE.path, body, &stale).is_err());
    let mut wrong_scope = trust(&sender);
    wrong_scope.scope = PeerScope::Operation("op".into());
    assert!(auth(&original, "POST", ROUTE.path, body, &wrong_scope).is_err());
    let mut cookie = original.clone();
    cookie.insert(
        header::COOKIE,
        HeaderValue::from_static("bilistream_session=x"),
    );
    assert!(auth(&cookie, "POST", ROUTE.path, body, &good).is_err());
    let mut duplicate = original.clone();
    duplicate.append(header::AUTHORIZATION, request.authorization.clone());
    assert!(auth(&duplicate, "POST", ROUTE.path, body, &good).is_err());
    let mut bearer = original.clone();
    bearer.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer legacy"),
    );
    assert!(auth(&bearer, "POST", ROUTE.path, body, &good).is_err());
    let restarted = PeerReceiver::new().unwrap();
    assert!(restarted
        .authenticate(
            &AdmittedRequest {
                headers: &original,
                method: "POST",
                path_and_query: ROUTE.path,
                route: ROUTE,
                body,
            },
            local.public(),
            &good,
        )
        .is_err());
    let peer = auth(&original, "POST", ROUTE.path, body, &good).unwrap();
    assert_eq!(peer.member_id(), sender.public().member_id);
    assert!(auth(&original, "POST", ROUTE.path, body, &good).is_err());
}

#[test]
fn recognition_authentication_does_not_admit_a_self_presented_key_on_other_routes() {
    let a = Installation::new();
    let b = Installation::new();
    let sender = a.identity();
    let local = b.identity();
    let receiver = PeerReceiver::new().unwrap();
    let hello = observed(&receiver, &local);
    let body = serde_json::to_vec(&RecognitionProbe {
        cluster_id: "test-cluster".into(),
        member_id: sender.public().member_id.clone(),
        public_key: sender.public().public_key.clone(),
    })
    .unwrap();
    let signed = sign_request(
        &sender,
        &hello,
        "test-cluster",
        7,
        PeerScope::Ordinary,
        ROUTE,
        &body,
    )
    .unwrap();
    let headers = headers(&signed);
    let request = AdmittedRequest {
        headers: &headers,
        method: ROUTE.method,
        path_and_query: ROUTE.path,
        route: ROUTE,
        body: &body,
    };
    assert!(receiver
        .authenticate_recognition(&request, local.public())
        .is_err());
    receiver
        .authenticate(&request, local.public(), &trust(&sender))
        .unwrap();
}

#[test]
fn responses_bind_request_status_type_and_raw_body() {
    let a = Installation::new();
    let b = Installation::new();
    let sender = a.identity();
    let local = b.identity();
    let receiver = PeerReceiver::new().unwrap();
    let hello = observed(&receiver, &local);
    let request = sign_request(
        &sender,
        &hello,
        "test-cluster",
        7,
        PeerScope::Ordinary,
        ROUTE,
        b"{}",
    )
    .unwrap();
    let peer = receiver
        .authenticate(
            &AdmittedRequest {
                headers: &headers(&request),
                method: "POST",
                path_and_query: ROUTE.path,
                route: ROUTE,
                body: b"{}",
            },
            local.public(),
            &trust(&sender),
        )
        .unwrap();
    let body = b"{\"success\":true}";
    let authorization = peer
        .sign_response(&local, 200, "application/json", body, ROUTE)
        .unwrap();
    let mut h = HeaderMap::new();
    h.insert(header::AUTHORIZATION, authorization);
    verify_response(
        &request,
        local.public(),
        &h,
        200,
        "application/json",
        body,
        ROUTE,
    )
    .unwrap();
    assert!(verify_response(
        &request,
        local.public(),
        &h,
        201,
        "application/json",
        body,
        ROUTE
    )
    .is_err());
    assert!(verify_response(&request, local.public(), &h, 200, "text/plain", body, ROUTE).is_err());
    assert!(verify_response(
        &request,
        local.public(),
        &h,
        200,
        "application/json",
        b"{}",
        ROUTE
    )
    .is_err());
    let other = sign_request(
        &sender,
        &hello,
        "test-cluster",
        7,
        PeerScope::Ordinary,
        ROUTE,
        b"{}",
    )
    .unwrap();
    assert!(verify_response(
        &other,
        local.public(),
        &h,
        200,
        "application/json",
        body,
        ROUTE
    )
    .is_err());
    h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    assert!(verify_response(
        &request,
        local.public(),
        &h,
        200,
        "application/json",
        body,
        ROUTE
    )
    .is_err());
    assert!(sign_request(
        &sender,
        &hello,
        "test-cluster",
        7,
        PeerScope::Ordinary,
        ROUTE,
        &vec![0; 1025]
    )
    .is_err());
    assert!(peer
        .sign_response(&local, 200, "application/json", &vec![0; 4097], ROUTE)
        .is_err());
}

#[test]
fn enrollment_transport_rejects_downgrades_dns_loopback_and_url_credentials() {
    for url in [
        "http://example.com",
        "http://localhost:80",
        "http://192.168.1.1",
        "https://u:p@example.com",
        "https://example.com?x=1",
        "https://example.com/#x",
        "ftp://example.com",
        "https://example.com\\@evil.com",
    ] {
        assert!(PeerEndpoint::parse(url, true).is_err(), "{url}");
    }
    assert!(PeerEndpoint::parse("http://127.0.0.1:1234", false).is_err());
    assert!(PeerEndpoint::parse("http://127.0.0.1:1234", true).is_ok());
    assert!(PeerEndpoint::parse("http://[::1]:1234", true).is_ok());
    let endpoint = PeerEndpoint::parse("https://example.com/prefix/", false).unwrap();
    assert_eq!(
        endpoint.route_url(ROUTE.path).unwrap().as_str(),
        "https://example.com/prefix/api/cluster/self-check"
    );
    assert!(endpoint
        .route_url("/api/cluster/self-check?alias=1")
        .is_err());
}

#[tokio::test]
async fn hello_http_classifies_origin_down_without_accepting_invalid_responses() {
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::post,
        Json, Router,
    };
    use std::io::ErrorKind;

    #[derive(Clone)]
    struct HelloState {
        identity: Arc<NodeIdentity>,
        receiver: Arc<PeerReceiver>,
    }
    async fn hello(
        State(state): State<HelloState>,
        Path(reply): Path<String>,
        Json(challenge): Json<HelloRequest>,
    ) -> Response {
        if let Ok(status) = reply.parse::<u16>() {
            return (StatusCode::from_u16(status).unwrap(), "upstream error").into_response();
        }
        if reply == "malformed" {
            return "invalid JSON".into_response();
        }
        let mut hello = state.receiver.hello(&state.identity, &challenge).unwrap();
        if reply == "tampered" {
            hello.elapsed_ms += 1;
        }
        let mut response = Json(hello).into_response();
        if reply.starts_with("compressed") {
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        if reply == "compressed-origin-down" {
            *response.status_mut() = StatusCode::BAD_GATEWAY;
        }
        response
    }
    let install = Installation::new();
    let identity = Arc::new(install.identity());
    let router = Router::new()
        .route("/{reply}/api/cluster/v1/hello", post(hello))
        .with_state(HelloState {
            identity: identity.clone(),
            receiver: Arc::new(PeerReceiver::new().unwrap()),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = PeerClient::new(Duration::from_secs(2)).unwrap();
    let cases = (502..=504)
        .chain(520..=530)
        .map(|status| (status.to_string(), ErrorKind::ConnectionAborted))
        .chain(
            [301, 307, 401, 403, 429, 500, 501, 505, 519, 531]
                .into_iter()
                .map(|status| (status.to_string(), ErrorKind::PermissionDenied)),
        )
        .chain([
            ("malformed".into(), ErrorKind::InvalidData),
            ("compressed".into(), ErrorKind::InvalidData),
            (
                "compressed-origin-down".into(),
                ErrorKind::ConnectionAborted,
            ),
            ("tampered".into(), ErrorKind::PermissionDenied),
        ]);
    for (reply, expected) in cases {
        let endpoint = PeerEndpoint::parse(&format!("http://{address}/{reply}"), true).unwrap();
        let error = client
            .hello(&endpoint, identity.public())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), expected, "pinned hello: {reply}");
        let error = client.discover(&endpoint).await.unwrap_err();
        assert_eq!(error.kind(), expected, "discovery hello: {reply}");
    }
    let endpoint = PeerEndpoint::parse(&format!("http://{address}/valid"), true).unwrap();
    client.hello(&endpoint, identity.public()).await.unwrap();
    assert_eq!(
        client.discover(&endpoint).await.unwrap(),
        *identity.public()
    );
    server.abort();
}

#[tokio::test]
async fn peer_client_never_follows_a_redirect() {
    use axum::{routing::post, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let hit_count = hits.clone();
    let router = Router::new()
        .route(
            HELLO_ROUTE,
            post(|| async {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, "/trap")],
                )
            }),
        )
        .route(
            "/trap",
            post(move || {
                let hits = hit_count.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    "trap"
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        PeerEndpoint::parse(&format!("http://{}", listener.local_addr().unwrap()), true).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let install = Installation::new();
    let identity = install.identity();
    let client = PeerClient::new(Duration::from_secs(2)).unwrap();
    assert!(client.hello(&endpoint, identity.public()).await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn hello_cap_rejects_an_otherwise_valid_chunked_json_response() {
    use axum::{body::Body, extract::State, response::Response, routing::post, Json, Router};
    #[derive(Clone)]
    struct HelloState {
        identity: Arc<NodeIdentity>,
        receiver: Arc<PeerReceiver>,
    }
    async fn huge_hello(
        State(s): State<HelloState>,
        Json(request): Json<HelloRequest>,
    ) -> Response {
        let mut bytes =
            serde_json::to_vec(&s.receiver.hello(&s.identity, &request).unwrap()).unwrap();
        bytes.extend_from_slice(&vec![b' '; HELLO_RESPONSE_BYTES]);
        let chunks = bytes
            .chunks(512)
            .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
            .collect::<Vec<_>>();
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .unwrap()
    }
    let install = Installation::new();
    let identity = Arc::new(install.identity());
    let router = Router::new()
        .route(HELLO_ROUTE, post(huge_hello))
        .with_state(HelloState {
            identity,
            receiver: Arc::new(PeerReceiver::new().unwrap()),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        PeerEndpoint::parse(&format!("http://{}", listener.local_addr().unwrap()), true).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = PeerClient::new(Duration::from_secs(2)).unwrap();
    assert_eq!(
        client.discover(&endpoint).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    server.abort();
}

#[tokio::test]
async fn signed_http_roundtrip_preserves_proxy_prefix_and_skips_response_compression() {
    use axum::{
        body::Bytes,
        extract::State,
        http::{Method, StatusCode, Uri},
        response::{IntoResponse, Response},
        routing::{get, post},
        Json, Router,
    };
    use tower_http::compression::CompressionLayer;
    #[derive(Clone)]
    struct ServerState {
        identity: Arc<NodeIdentity>,
        receiver: Arc<PeerReceiver>,
        sender: PublicIdentity,
    }
    async fn hello(
        State(state): State<ServerState>,
        Json(challenge): Json<HelloRequest>,
    ) -> Json<HelloResponse> {
        Json(state.receiver.hello(&state.identity, &challenge).unwrap())
    }
    async fn request(
        State(state): State<ServerState>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let trust = TrustSnapshot {
            cluster_id: "test-cluster".into(),
            revisions: vec![7],
            scope: PeerScope::Ordinary,
            members: vec![state.sender.clone()],
        };
        let peer = match state.receiver.authenticate(
            &AdmittedRequest {
                headers: &headers,
                method: method.as_str(),
                path_and_query: uri.path_and_query().unwrap().as_str(),
                route: ROUTE,
                body: &body,
            },
            state.identity.public(),
            &trust,
        ) {
            Ok(peer) => peer,
            Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
        };
        let body = serde_json::to_vec(&json!({"data":"a".repeat(2048)})).unwrap();
        let signed = peer
            .sign_response(&state.identity, 200, "application/json", &body, ROUTE)
            .unwrap();
        (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                ),
                (header::AUTHORIZATION, signed),
            ],
            body,
        )
            .into_response()
    }
    let a = Installation::new();
    let b = Installation::new();
    let sender = a.identity();
    let local = Arc::new(b.identity());
    let receiver = Arc::new(PeerReceiver::new().unwrap());
    let router = Router::new()
        .nest(
            "/prefix",
            Router::new()
                .route(HELLO_ROUTE, post(hello))
                .route(ROUTE.path, post(request))
                .route(
                    "/browser",
                    get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "b".repeat(2048)) }),
                ),
        )
        .with_state(ServerState {
            identity: local.clone(),
            receiver: receiver.clone(),
            sender: sender.public().clone(),
        })
        .layer(CompressionLayer::new().compress_when(PeerCompression));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = PeerEndpoint::parse(&format!("http://{address}/prefix/"), true).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = PeerClient::new(Duration::from_secs(2)).unwrap();
    assert_eq!(client.discover(&endpoint).await.unwrap(), *local.public());
    let response = client
        .send(
            &sender,
            &endpoint,
            local.public(),
            "test-cluster",
            7,
            PeerScope::Ordinary,
            ROUTE,
            b"{}".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&response.body).unwrap()["data"]
            .as_str()
            .unwrap()
            .len(),
        2048
    );
    // Even a proxy/client which requests gzip must get the signed raw bytes.
    let hello = observed(&receiver, &local);
    let signed = sign_request(
        &sender,
        &hello,
        "test-cluster",
        7,
        PeerScope::Ordinary,
        ROUTE,
        b"{}",
    )
    .unwrap();
    let raw = reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()
        .unwrap();
    let response = raw
        .post(endpoint.route_url(ROUTE.path).unwrap())
        .header(header::AUTHORIZATION, signed.authorization.clone())
        .header(header::ACCEPT_ENCODING, "gzip")
        .body("{}")
        .send()
        .await
        .unwrap();
    let headers = response.headers().clone();
    assert!(!headers.contains_key(header::CONTENT_ENCODING));
    let body = response.bytes().await.unwrap();
    verify_response(
        &signed,
        local.public(),
        &headers,
        200,
        "application/json",
        &body,
        ROUTE,
    )
    .unwrap();
    let browser = raw
        .get(format!("http://{address}/prefix/browser"))
        .header(header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(browser.headers()[header::CONTENT_ENCODING], "gzip");
    server.abort();
}
