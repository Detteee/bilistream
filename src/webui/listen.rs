use super::sessions::{normalize_password, PasswordMutation, PasswordSnapshot, Sessions};
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const COOKIE_NAME: &str = "bilistream_session";

static LISTEN: OnceLock<ListenConfig> = OnceLock::new();

#[derive(Clone, Default)]
pub(crate) struct PasswordBootstrap {
    pub(crate) password: Option<String>,
    pub(crate) file: Option<std::path::PathBuf>,
    pub(crate) restart_file: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for PasswordBootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordBootstrap(<redacted>)")
    }
}

impl PasswordBootstrap {
    fn resolve(self, store: Arc<crate::storage::Store>) -> std::io::Result<Sessions> {
        // Internal one-use handoffs are consumed even when saved auth supersedes them.
        let handoff = self
            .restart_file
            .as_deref()
            .map(super::restart::consume_restart_password)
            .transpose()
            .map_err(std::io::Error::other)?;
        Sessions::open(store, || {
            let value = if let Some(handoff) = handoff {
                Some(handoff)
            } else if let Some(path) = self.file {
                Some(super::restart::read_password_file(&path).map_err(std::io::Error::other)?)
            } else {
                self.password
            };
            value.map(|p| normalize_password(&p)).transpose()
        })
    }
}

pub(crate) fn install_bootstrap(bind: &str, bootstrap: PasswordBootstrap) -> Result<(), String> {
    LISTEN
        .set(ListenConfig {
            bind: parse_bind(bind)?,
            bootstrap,
        })
        .map_err(|_| "Web UI listen address already configured".to_owned())
}

#[derive(Clone, Debug)]
struct ListenConfig {
    bind: IpAddr,
    bootstrap: PasswordBootstrap,
}

/// Parses `--bind` / `BILISTREAM_BIND`. `localhost` is `127.0.0.1`.
pub fn parse_bind(input: &str) -> Result<IpAddr, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("bind address is empty".to_string());
    }
    if input.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    input
        .parse::<IpAddr>()
        .map_err(|e| format!("invalid bind address '{input}': {e}"))
}

/// Installs the process-wide listen address and optional Web UI password.
///
/// Safe default when never called: `127.0.0.1` and no password.
pub fn install_listen(bind: &str, password: Option<String>) -> Result<(), String> {
    LISTEN
        .set(make_listen_config(bind, password)?)
        .map_err(|_| "Web UI listen address already configured".to_string())
}

fn make_listen_config(bind: &str, password: Option<String>) -> Result<ListenConfig, String> {
    let bind = parse_bind(bind)?;
    let password = password.filter(|p| !p.trim().is_empty());
    Ok(ListenConfig {
        bind,
        bootstrap: PasswordBootstrap {
            password,
            ..Default::default()
        },
    })
}

pub fn listen_bind() -> IpAddr {
    LISTEN
        .get()
        .map(|config| config.bind)
        .unwrap_or(DEFAULT_BIND)
}

/// Current committed requirement; storage failures fail closed.
pub fn password_required() -> bool {
    crate::storage::global()
        .and_then(|store| Sessions::open(store, || Ok(None)))
        .and_then(|sessions| sessions.snapshot())
        .map_or(true, |snapshot| snapshot.password.is_some())
}

pub(crate) async fn prepare_auth(bind: IpAddr) -> std::io::Result<Arc<AuthState>> {
    tokio::task::spawn_blocking(move || {
        // Holds, identity bindings and recovery state load before any monitor.
        let store = crate::storage::global()?;
        crate::cluster::membership::Membership::new(store.clone()).initialize()?;
        let bootstrap = LISTEN
            .get()
            .map(|config| config.bootstrap.clone())
            .unwrap_or_else(|| PasswordBootstrap {
                password: std::env::var("BILISTREAM_PASSWORD")
                    .ok()
                    .filter(|p| !p.trim().is_empty()),
                ..Default::default()
            });
        let sessions = bootstrap.resolve(store)?;
        AuthState::from_sessions(
            sessions,
            crate::cluster::membership::Runtime::process()?,
            bind,
        )
    })
    .await
    .map_err(std::io::Error::other)?
}

pub(crate) struct AuthState {
    bind: IpAddr,
    pub(super) sessions: Sessions,
    /// Boot nonce and replay window for signed peer requests on this listener.
    peer: crate::cluster::peer_auth::PeerReceiver,
    runtime: crate::cluster::membership::Runtime,
    pub(super) changed: tokio::sync::watch::Sender<()>,
    throttle: Mutex<LoginThrottle>,
}

impl AuthState {
    #[cfg(test)]
    pub(crate) fn open(
        store: Arc<crate::storage::Store>,
        password: Option<String>,
    ) -> std::io::Result<Arc<Self>> {
        let runtime = crate::cluster::membership::Runtime::new(
            crate::cluster::membership::Membership::new(store.clone()),
            crate::cluster::peer_call::peer_client()?,
            Arc::new(|| Box::pin(async { Ok(()) })),
        );
        let sessions = Sessions::open(store, || Ok(password))?;
        Self::from_sessions(sessions, runtime, DEFAULT_BIND)
    }

    fn from_sessions(
        sessions: Sessions,
        runtime: crate::cluster::membership::Runtime,
        bind: IpAddr,
    ) -> std::io::Result<Arc<Self>> {
        let snapshot = sessions.snapshot()?;
        if !bind.is_loopback() && snapshot.password.is_none() {
            return Err(std::io::Error::other(
                "监听非本机地址时必须先设置 Web UI 访问密码",
            ));
        }
        Ok(Arc::new(Self {
            bind,
            sessions,
            peer: crate::cluster::peer_auth::PeerReceiver::new()?,
            runtime,
            throttle: Mutex::default(),
            changed: tokio::sync::watch::channel(()).0,
        }))
    }

    pub(super) fn access(&self, headers: &HeaderMap) -> std::io::Result<(PasswordSnapshot, bool)> {
        self.sessions.access(
            session_from_headers(headers).as_deref(),
            crate::storage::now(),
        )
    }

    pub(crate) fn notify_changed(&self) {
        self.changed.send_replace(());
    }

    pub(crate) fn prepare_create(
        &self,
        headers: &HeaderMap,
        peer: Option<SocketAddr>,
        password: &str,
        initial: bool,
    ) -> Result<PasswordMutation, AuthError> {
        if !same_origin(headers) || !local_creation(self.bind, headers, peer) {
            return Err(AuthError(StatusCode::FORBIDDEN, "请在本机地址设置首个密码"));
        }
        let snapshot = self
            .sessions
            .snapshot()
            .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "无法读取面板密码"))?;
        if snapshot.password.is_some() {
            return Err(AuthError(
                StatusCode::CONFLICT,
                "密码已设置，请在设置页更改",
            ));
        }
        let password = self.validate_new(password)?;
        PasswordMutation::new(snapshot.revision, Some(password), initial)
            .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "无法准备面板密码"))
    }

    fn validate_new(&self, value: &str) -> Result<String, AuthError> {
        normalize_password(value)
            .map_err(|_| AuthError(StatusCode::BAD_REQUEST, "密码不能为空或超过 64 KiB"))
    }

    pub(crate) fn peer_hello(
        &self,
        identity: &crate::cluster::peer_auth::NodeIdentity,
        request: &crate::cluster::peer_auth::HelloRequest,
    ) -> std::io::Result<crate::cluster::peer_auth::HelloResponse> {
        self.peer.hello(identity, request)
    }

    pub(crate) fn runtime(&self) -> &crate::cluster::membership::Runtime {
        &self.runtime
    }

    /// Bootstrap pairing reuses the panel password check and login throttle,
    /// keyed by the socket peer. It never issues a browser session.
    pub(crate) fn check_peer_password(
        &self,
        source: IpAddr,
        password: &str,
    ) -> Result<Option<u64>, (StatusCode, Option<Duration>)> {
        let snapshot = self
            .sessions
            .snapshot()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, None))?;
        let Some(expected) = snapshot.password.as_deref() else {
            return Err((StatusCode::FORBIDDEN, None));
        };
        let correct = password.len() <= super::sessions::PASSWORD_LIMIT
            && secret_eq(password.trim(), expected);
        match self
            .throttle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .attempt(source, correct, Instant::now())
        {
            Ok(true) => Ok(snapshot.revision),
            Ok(false) => Err((StatusCode::FORBIDDEN, None)),
            Err(retry) => Err((StatusCode::TOO_MANY_REQUESTS, Some(retry))),
        }
    }

    /// Browser membership mutations: same origin and still the credential
    /// generation that admitted this request.
    pub(crate) fn browser_mutation_allowed(
        &self,
        headers: &HeaderMap,
        generation: Option<AuthGeneration>,
    ) -> bool {
        same_origin(headers)
            && self
                .sessions
                .snapshot()
                .is_ok_and(|snapshot| generation.is_some_and(|g| g.0 == snapshot.revision))
    }

    pub(crate) fn password_configured(&self) -> bool {
        self.sessions
            .snapshot()
            .is_ok_and(|snapshot| snapshot.password.is_some())
    }

    /// Browser authority only. Peer-only and operation paths are never
    /// reachable with a cookie or on passwordless loopback.
    fn browser_allows(&self, logical: &str, headers: &HeaderMap) -> bool {
        !crate::cluster::peer_call::browser_forbidden(logical)
            && self.access(headers).is_ok_and(|(_, valid)| valid)
    }

    async fn peer_request(
        self: &Arc<Self>,
        request: Request,
        next: Next,
        route: crate::cluster::peer_auth::RoutePolicy,
        class: crate::cluster::peer_call::RouteClass,
        path_and_query: String,
    ) -> Response {
        use crate::cluster::membership::{operation_intent, operation_trust, trust_snapshot};
        let (parts, body) = request.into_parts();
        // Byte cap before any parsing; bodies are hashed exactly as received.
        let Ok(bytes) = axum::body::to_bytes(body, route.max_request_bytes).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        // Once admitted, the handler and every child task it awaits own the
        // membership guard together. Dropping the HTTP future must not let a
        // queued node-mode/config write outlive the revision that authorized it.
        // Read the bounded request body first so disconnected uploaders own no
        // background task or membership guard.
        let auth = Arc::clone(self);
        tokio::spawn(async move {
            let operation = class == crate::cluster::peer_call::RouteClass::Operation;
            let _gate = if operation {
                None
            } else {
                Some(crate::cluster::membership::MEMBERSHIP_GATE.read().await)
            };
            let store = auth.runtime.membership.store();
            let Ok(identity) = auth.runtime.membership.identity() else {
                return StatusCode::UNAUTHORIZED.into_response();
            };
            let trust = if operation {
                operation_intent(route.path, &bytes)
                    .and_then(|intent| operation_trust(store, &intent))
            } else {
                trust_snapshot(store)
            };
            let Ok(trust) = trust else {
                return StatusCode::UNAUTHORIZED.into_response();
            };
            let Ok(peer) = auth.peer.authenticate(
                &crate::cluster::peer_auth::AdmittedRequest {
                    headers: &parts.headers,
                    method: parts.method.as_str(),
                    path_and_query: &path_and_query,
                    route,
                    body: &bytes,
                },
                identity.public(),
                &trust,
            ) else {
                return StatusCode::UNAUTHORIZED.into_response();
            };
            let mut request = Request::from_parts(parts, axum::body::Body::from(bytes));
            request
                .extensions_mut()
                .insert(crate::cluster::peer_call::AuthenticatedNode {
                    member_id: peer.member_id().to_owned(),
                    node_id: crate::cluster::peer_call::member_node_id(store, peer.member_id())
                        .unwrap_or_default(),
                });
            let (mut parts, body) = next.run(request).await.into_parts();
            let Ok(bytes) = axum::body::to_bytes(body, route.max_response_bytes).await else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            if !parts.headers.contains_key(header::CONTENT_TYPE) {
                parts.headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                );
            }
            parts.headers.remove(header::CONTENT_LENGTH);
            let content_type = parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            match peer.sign_response(
                &identity,
                parts.status.as_u16(),
                &content_type,
                &bytes,
                route,
            ) {
                Ok(signature) => {
                    parts.headers.insert(header::AUTHORIZATION, signature);
                    Response::from_parts(parts, axum::body::Body::from(bytes))
                }
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        })
        .await
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    /// Verify the probe signature with the key it carries. Do not insert
    /// [`AuthenticatedNode`](crate::cluster::peer_call::AuthenticatedNode): a
    /// recognition admission is not permission to call any other peer route.
    async fn recognition_request(
        &self,
        request: Request,
        next: Next,
        route: crate::cluster::peer_auth::RoutePolicy,
        path_and_query: String,
    ) -> Response {
        let (parts, body) = request.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, route.max_request_bytes).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        let Ok(identity) = self.runtime.membership.identity() else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        let Ok((peer, probe)) = self.peer.authenticate_recognition(
            &crate::cluster::peer_auth::AdmittedRequest {
                headers: &parts.headers,
                method: parts.method.as_str(),
                path_and_query: &path_and_query,
                route,
                body: &bytes,
            },
            identity.public(),
        ) else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        let mut request = Request::from_parts(parts, axum::body::Body::from(bytes));
        request
            .extensions_mut()
            .insert(crate::cluster::peer_auth::VerifiedRecognition(probe));
        let (mut parts, body) = next.run(request).await.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, route.max_response_bytes).await else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        if !parts.headers.contains_key(header::CONTENT_TYPE) {
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
        }
        parts.headers.remove(header::CONTENT_LENGTH);
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        match peer.sign_response(
            &identity,
            parts.status.as_u16(),
            &content_type,
            &bytes,
            route,
        ) {
            Ok(signature) => {
                parts.headers.insert(header::AUTHORIZATION, signature);
                Response::from_parts(parts, axum::body::Body::from(bytes))
            }
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

/// Router-relative and nested paths map to one logical `/api/...` route.
fn logical_path(path: &str) -> String {
    if path.starts_with("/api/") {
        path.to_owned()
    } else {
        format!("/api{path}")
    }
}

#[derive(Serialize)]
pub struct AuthStatus {
    pub required: bool,
    pub authenticated: bool,
    pub can_create_password: bool,
    pub can_clear_password: bool,
}

pub(super) async fn auth_status(
    Extension(auth): Extension<Arc<AuthState>>,
    request: Request,
) -> Json<AuthStatus> {
    let access = auth.access(request.headers());
    let required = access
        .as_ref()
        .map_or(true, |(state, _)| state.password.is_some());
    let authenticated = access.is_ok_and(|(_, valid)| valid);
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0);
    Json(AuthStatus {
        required,
        authenticated,
        can_create_password: !required && local_creation(auth.bind, request.headers(), peer),
        can_clear_password: required && authenticated && auth.bind.is_loopback(),
    })
}

fn host_url(headers: &HeaderMap, scheme: &str) -> Option<reqwest::Url> {
    if headers.get_all(header::HOST).iter().count() != 1 {
        return None;
    }
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let authority: axum::http::uri::Authority = host.parse().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let url = reqwest::Url::parse(&format!("{scheme}://{authority}")).ok()?;
    (url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
        .then_some(url)
}

fn same_origin(headers: &HeaderMap) -> bool {
    if headers.get_all(header::ORIGIN).iter().count() != 1 {
        return false;
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.chars().any(char::is_whitespace) && !value.contains('\\'))
        .and_then(|value| reqwest::Url::parse(value).ok())
    else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    host_url(headers, origin.scheme()).is_some_and(|host| {
        host.host() == origin.host()
            && host.port_or_known_default() == origin.port_or_known_default()
    })
}

fn local_creation(bind: IpAddr, headers: &HeaderMap, peer: Option<SocketAddr>) -> bool {
    if !bind.is_loopback()
        || !peer.is_some_and(|peer| peer.ip().is_loopback())
        || headers.keys().any(|key| {
            key.as_str().starts_with("x-forwarded-")
                || matches!(
                    key.as_str(),
                    "forwarded"
                        | "x-forwarded"
                        | "via"
                        | "x-real-ip"
                        | "true-client-ip"
                        | "cf-connecting-ip"
                        | "client-ip"
                        | "x-client-ip"
                )
        })
    {
        return false;
    }
    host_url(headers, "http")
        .and_then(|host| host.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

/// A rejected password request: HTTP status plus an operator-facing message.
pub(crate) struct AuthError(StatusCode, &'static str);

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        auth_error(self.0, self.1)
    }
}

fn auth_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"success": false, "message": message})),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum PasswordBody {
    Create {
        new_password: String,
    },
    Change {
        current_password: String,
        new_password: String,
    },
    Clear {
        current_password: String,
    },
}

pub(super) async fn change_password(
    Extension(auth): Extension<Arc<AuthState>>,
    headers: HeaderMap,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(body): Json<PasswordBody>,
) -> Response {
    if !same_origin(&headers) {
        return auth_error(StatusCode::FORBIDDEN, "请求来源与面板地址不一致");
    }
    let peer = connection.map(|Extension(ConnectInfo(peer))| peer);
    let mutation = match body {
        PasswordBody::Create { new_password } => {
            match auth.prepare_create(&headers, peer, &new_password, false) {
                Ok(mutation) => mutation,
                Err(error) => return error.into_response(),
            }
        }
        other => {
            let (snapshot, valid) = match auth.access(&headers) {
                Ok(access) => access,
                Err(_) => return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "无法读取面板密码"),
            };
            if !valid {
                return auth_error(StatusCode::UNAUTHORIZED, "请重新登录");
            }
            let Some(expected) = snapshot.password else {
                return auth_error(StatusCode::CONFLICT, "密码状态已更改，请刷新");
            };
            let (current, replacement) = match other {
                PasswordBody::Change {
                    current_password,
                    new_password,
                } => {
                    let new = match auth.validate_new(&new_password) {
                        Ok(value) => value,
                        Err(error) => return error.into_response(),
                    };
                    (current_password, Some(new))
                }
                PasswordBody::Clear { current_password } => {
                    if !auth.bind.is_loopback() {
                        return auth_error(StatusCode::FORBIDDEN, "远程监听时不能移除密码");
                    }
                    (current_password, None)
                }
                PasswordBody::Create { .. } => unreachable!(),
            };
            let accepted = auth
                .throttle
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .attempt(
                    peer.map_or(DEFAULT_BIND, |addr| addr.ip()),
                    secret_eq(current.trim(), &expected),
                    Instant::now(),
                );
            match accepted {
                Ok(true) => {}
                Ok(false) => return auth_error(StatusCode::FORBIDDEN, "当前密码错误"),
                Err(retry) => {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, retry.as_secs().max(1).to_string())],
                        Json(serde_json::json!({"success":false,"message":"尝试过多，请稍后重试"})),
                    )
                        .into_response()
                }
            }
            match PasswordMutation::new(snapshot.revision, replacement, false) {
                Ok(mutation) => mutation,
                Err(_) => return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "无法准备面板密码"),
            }
        }
    };
    let saved_auth = auth.clone();
    match tokio::task::spawn_blocking(move || {
        saved_auth.sessions.mutate(mutation)?;
        saved_auth.notify_changed();
        Ok::<_, std::io::Error>(())
    })
    .await
    {
        Ok(Ok(())) => {
            let mut response = Json(serde_json::json!({"success":true})).into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, expired_cookie());
            response
        }
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
            auth_error(StatusCode::CONFLICT, "密码状态已更改，请刷新")
        }
        _ => auth_error(StatusCode::INTERNAL_SERVER_ERROR, "无法保存面板密码"),
    }
}

fn expired_cookie() -> HeaderValue {
    HeaderValue::from_static("bilistream_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

#[derive(Clone, Copy)]
pub(crate) struct AuthGeneration(pub Option<u64>);

#[derive(Deserialize)]
pub struct LoginBody {
    #[serde(default)]
    password: String,
}

const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_FAILURE_LIMIT: u8 = 5;
const LOGIN_SOURCE_LIMIT: usize = 1024;

#[derive(Default)]
struct LoginThrottle {
    failures: HashMap<IpAddr, (u8, Instant)>,
}

impl LoginThrottle {
    fn attempt(&mut self, source: IpAddr, correct: bool, now: Instant) -> Result<bool, Duration> {
        self.failures.retain(|_, (_, until)| *until > now);
        if let Some((count, until)) = self.failures.get(&source) {
            if *count >= LOGIN_FAILURE_LIMIT {
                return Err(until.saturating_duration_since(now));
            }
        }
        if correct {
            self.failures.remove(&source);
            return Ok(true);
        }
        if !self.failures.contains_key(&source) && self.failures.len() >= LOGIN_SOURCE_LIMIT {
            return Err(LOGIN_WINDOW);
        }
        let (count, _) = self
            .failures
            .entry(source)
            .or_insert((0, now + LOGIN_WINDOW));
        *count += 1;
        if *count == LOGIN_FAILURE_LIMIT {
            tracing::warn!(%source, "Web UI 登录失败过多，暂时限制该来源");
        }
        Ok(false)
    }
}

pub(super) async fn login(
    Extension(auth): Extension<Arc<AuthState>>,
    headers: HeaderMap,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(body): Json<LoginBody>,
) -> Response {
    let snapshot = match auth.sessions.snapshot() {
        Ok(snapshot) => snapshot,
        Err(_) => return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "无法读取面板密码"),
    };
    let Some(expected) = snapshot.password.as_deref() else {
        return Json(serde_json::json!({ "success": true, "required": false })).into_response();
    };

    // Use the socket peer; untrusted X-Forwarded-For must not bypass the limit.
    let source = connection.map_or(DEFAULT_BIND, |Extension(ConnectInfo(addr))| addr.ip());
    let accepted = auth
        .throttle
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .attempt(
            source,
            secret_eq(body.password.trim(), expected),
            Instant::now(),
        );
    let correct = match accepted {
        Ok(correct) => correct,
        Err(retry) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry.as_secs().max(1).to_string())],
                Json(serde_json::json!({"success":false,"message":"登录尝试过多，请稍后重试"})),
            )
                .into_response()
        }
    };
    if correct {
        let previous = session_from_headers(&headers);
        let session = tokio::task::spawn_blocking(move || {
            auth.sessions.issue(
                snapshot.revision,
                previous.as_deref(),
                crate::storage::now(),
            )
        })
        .await;
        let token = match session {
            Ok(Ok(token)) => token,
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return auth_error(StatusCode::CONFLICT, "密码已更改，请重新登录")
            }
            _ => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"success":false,"message":"无法保存登录会话"})),
                )
                    .into_response()
            }
        };
        let mut response = Json(serde_json::json!({ "success": true })).into_response();
        response
            .headers_mut()
            .insert(header::SET_COOKIE, session_set_cookie_header(&token));
        response
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "success": false, "message": "密码错误" })),
        )
            .into_response()
    }
}

pub(super) async fn logout(
    Extension(auth): Extension<Arc<AuthState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = session_from_headers(&headers) {
        let result = tokio::task::spawn_blocking(move || {
            auth.sessions.revoke(&token)?;
            auth.notify_changed();
            Ok::<_, std::io::Error>(())
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"success":false,"message":"无法注销登录会话，请重试"})),
            )
                .into_response();
        }
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(cookie) = HeaderValue::from_str(&format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
    )) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

pub(super) async fn require_webui_auth(
    Extension(auth): Extension<Arc<AuthState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if is_public_api_path(request.uri().path()) {
        return Ok(next.run(request).await);
    }
    use crate::cluster::peer_call::{peer_route, RouteClass};
    let logical = logical_path(request.uri().path());
    let route = peer_route(request.method().as_str(), &logical);
    if let Some((_, RouteClass::Bootstrap)) = route {
        if request.headers().contains_key(header::AUTHORIZATION) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        return Ok(next.run(request).await);
    }
    if let Some((route, RouteClass::Recognition)) = route {
        // Ordinary membership authentication would reject a removed node's stale
        // revision before looking at the key. This probe verifies the key in the
        // body and grants no peer authority.
        if !request.headers().contains_key(header::AUTHORIZATION) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let path_and_query = match request.uri().query() {
            Some(query) => format!("{logical}?{query}"),
            None => logical,
        };
        return Ok(auth
            .recognition_request(request, next, route, path_and_query)
            .await);
    }
    // A supplied Authorization is always peer authority and never falls back
    // to a browser session or passwordless local access.
    if request.headers().contains_key(header::AUTHORIZATION) {
        let Some((route, class)) = route else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        let path_and_query = match request.uri().query() {
            Some(query) => format!("{logical}?{query}"),
            None => logical,
        };
        return Ok(auth
            .peer_request(request, next, route, class, path_and_query)
            .await);
    }

    if auth.browser_allows(&logical, request.headers()) {
        // Capture the authority that admitted the stream before its handler subscribes.
        let (snapshot, valid) = auth
            .access(request.headers())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if request
            .uri()
            .path()
            .trim_end_matches('/')
            .ends_with("/events")
            && !valid
        {
            return Err(StatusCode::UNAUTHORIZED);
        }
        request
            .extensions_mut()
            .insert(AuthGeneration(snapshot.revision));
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub(crate) fn is_public_api_path(path: &str) -> bool {
    matches!(
        path,
        "/health"
            | "/api/health"
            | "/auth"
            | "/api/auth"
            | "/login"
            | "/api/login"
            | "/logout"
            | "/api/logout"
    )
}

pub(crate) fn session_set_cookie_header(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        super::sessions::LIFETIME_SECS
    ))
    .expect("generated hex session cookie")
}

fn session_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find_map(|header| cookie_value(header, COOKIE_NAME))
}

fn cookie_value(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        let (key, value) = part.split_once('=')?;
        if key.trim() != name {
            continue;
        }
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn secret_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
#[path = "listen_peer_tests.rs"]
mod peer_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (std::path::PathBuf, Arc<crate::storage::Store>) {
        let root = std::env::temp_dir().join(format!("bilistream-{name}-{}", std::process::id()));
        let store =
            crate::storage::Store::open(root.join("data"), root.join("keys/master"), None).unwrap();
        (root, store)
    }

    fn test_runtime(store: &Arc<crate::storage::Store>) -> crate::cluster::membership::Runtime {
        crate::cluster::membership::Runtime::new(
            crate::cluster::membership::Membership::new(store.clone()),
            crate::cluster::peer_call::peer_client().unwrap(),
            Arc::new(|| Box::pin(async { Ok(()) })),
        )
    }

    fn app(auth: Arc<AuthState>) -> axum::Router {
        use axum::routing::{get, post};
        axum::Router::new()
            .route("/api/auth", get(auth_status))
            .route("/api/auth/password", post(change_password))
            .route(
                "/api/setup/save-config",
                post(super::super::api::save_setup_config),
            )
            .route("/api/login", post(login))
            .route("/api/events", get(super::super::events::sse_events))
            .layer(axum::middleware::from_fn(require_webui_auth))
            .layer(Extension(auth))
            .with_state(crate::AppState::new())
    }

    fn request(path: &str, body: serde_json::Value, cookie: Option<&str>) -> Request {
        let mut request = Request::builder()
            .uri(path)
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "localhost:3150")
            .header(header::ORIGIN, "http://localhost:3150");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let mut request = request
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
        ));
        request
    }

    #[tokio::test]
    async fn both_creation_routes_enforce_peer_host_origin_and_proxy_guards() {
        use tower::ServiceExt;
        for (index, (host, origin, peer, proxy, accepted)) in [
            (
                "localhost:3150",
                "http://localhost:3150",
                Some("127.0.0.1:20"),
                None,
                true,
            ),
            (
                "127.0.0.2:3150",
                "http://127.0.0.2:3150",
                Some("127.0.0.2:20"),
                None,
                true,
            ),
            (
                "[::1]:3150",
                "http://[::1]:3150",
                Some("[::1]:20"),
                None,
                true,
            ),
            ("localhost:3150", "http://localhost:3150", None, None, false),
            (
                "localhost:3150",
                "http://localhost:3150",
                Some("192.0.2.1:20"),
                None,
                false,
            ),
            (
                "attacker.test:3150",
                "http://attacker.test:3150",
                Some("127.0.0.1:20"),
                None,
                false,
            ),
            (
                "localhost:3150",
                "http://localhost:3151",
                Some("127.0.0.1:20"),
                None,
                false,
            ),
            ("localhost:3150", "null", Some("127.0.0.1:20"), None, false),
            (
                "localhost:3150",
                "http://user@localhost:3150",
                Some("127.0.0.1:20"),
                None,
                false,
            ),
            (
                "localhost:3150",
                "http://localhost:3150/path",
                Some("127.0.0.1:20"),
                None,
                false,
            ),
            (
                "localhost:3150",
                "http://localhost:3150",
                Some("127.0.0.1:20"),
                Some("x-forwarded-for"),
                false,
            ),
            (
                "localhost:3150",
                "http://localhost:3150",
                Some("127.0.0.1:20"),
                Some("forwarded"),
                false,
            ),
            (
                "localhost:3150",
                "http://localhost:3150",
                Some("127.0.0.1:20"),
                Some("x-real-ip"),
                false,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let (root, store) = store(&format!("password-guard-{index}"));
            let auth = AuthState::open(store.clone(), None).unwrap();
            let app = app(auth.clone());
            // Invalid setup room exercises the same creation guard without global config or network.
            for path in ["/api/setup/save-config", "/api/auth/password"] {
                let body = if path.contains("setup") {
                    serde_json::json!({"room":0,"auto_cover":false,"interval":60,"anti_collision":false,"enable_danmaku_command":false,"enable_lol_monitor":false,"panel_password":"synthetic-password"})
                } else {
                    serde_json::json!({"action":"create","new_password":"synthetic-password"})
                };
                let mut req = request(path, body, None);
                req.headers_mut()
                    .insert(header::HOST, host.parse().unwrap());
                req.headers_mut()
                    .insert(header::ORIGIN, origin.parse().unwrap());
                req.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
                if let Some(peer) = peer {
                    req.extensions_mut()
                        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
                }
                if let Some(proxy) = proxy {
                    req.headers_mut().insert(
                        axum::http::HeaderName::from_static(proxy),
                        "synthetic".parse().unwrap(),
                    );
                }
                let response = app.clone().oneshot(req).await.unwrap();
                assert_eq!(
                    response.status(),
                    if accepted {
                        StatusCode::OK
                    } else {
                        StatusCode::FORBIDDEN
                    },
                    "case {index} {path}"
                );
            }
            assert_eq!(
                auth.sessions.snapshot().unwrap().password.is_some(),
                accepted
            );
            drop((app, auth, store));
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn password_changes_revoke_sessions_require_current_and_keep_peer_authority_separate() {
        use tower::ServiceExt;
        let (root, store) = store("password-mutations");
        let cluster_key = "synthetic-cluster-key-0123456789abcdef";
        let auth = AuthState::open(store.clone(), None).unwrap();
        let app = app(auth.clone());
        // Capability GET does not need Origin; it still requires the actual local peer and Host.
        let mut req = request("/api/auth", serde_json::json!({}), None);
        *req.method_mut() = axum::http::Method::GET;
        req.headers_mut().remove(header::ORIGIN);
        let response = app.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status["can_create_password"], true);
        for body in [serde_json::json!({"action":"create","new_password":" "})] {
            assert_eq!(
                app.clone()
                    .oneshot(request("/api/auth/password", body, None))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let create = serde_json::json!({"action":"create","new_password":"first"});
        let mut peer = request("/api/auth/password", create.clone(), None);
        peer.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {cluster_key}").parse().unwrap(),
        );
        assert_eq!(
            app.clone().oneshot(peer).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/api/auth/password", create, None))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let login = app
            .clone()
            .oneshot(request(
                "/api/login",
                serde_json::json!({"password":"first"}),
                None,
            ))
            .await
            .unwrap();
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        let wrong = serde_json::json!({"action":"change","current_password":"wrong","new_password":"second"});
        assert_eq!(
            app.clone()
                .oneshot(request("/api/auth/password", wrong, Some(&cookie)))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let same = serde_json::json!({"action":"change","current_password":"first","new_password":"first"});
        let mut cross = request("/api/auth/password", same.clone(), Some(&cookie));
        cross
            .headers_mut()
            .insert(header::ORIGIN, "http://other.test".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(cross).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let changed = app
            .clone()
            .oneshot(request("/api/auth/password", same.clone(), Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(changed.status(), StatusCode::OK);
        assert!(changed.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
        assert_eq!(
            app.clone()
                .oneshot(request("/api/auth/password", same, Some(&cookie)))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let login = app
            .clone()
            .oneshot(request(
                "/api/login",
                serde_json::json!({"password":"first"}),
                None,
            ))
            .await
            .unwrap();
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        let clear = serde_json::json!({"action":"clear","current_password":"first"});
        let remote_auth = AuthState::from_sessions(
            Sessions::open(store.clone(), || panic!()).unwrap(),
            test_runtime(&store),
            "0.0.0.0".parse().unwrap(),
        )
        .unwrap();
        let remote = self::app(remote_auth.clone());
        assert_eq!(
            remote
                .clone()
                .oneshot(request("/api/auth/password", clear.clone(), Some(&cookie)))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/api/auth/password", clear, Some(&cookie)))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert!(auth.sessions.snapshot().unwrap().password.is_none());
        assert!(auth.sessions.snapshot().unwrap().revision.is_some());
        assert!(AuthState::from_sessions(
            Sessions::open(store.clone(), || panic!()).unwrap(),
            test_runtime(&store),
            "0.0.0.0".parse().unwrap()
        )
        .is_err());
        drop((app, auth, remote, remote_auth, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_disabled_and_handoff_bootstrap_precedence_fail_closed() {
        let (root, store) = store("password-bootstrap");
        let sessions = PasswordBootstrap::default().resolve(store.clone()).unwrap();
        assert!(sessions.snapshot().unwrap().revision.is_none());
        let missing = root.join("missing");
        assert!(PasswordBootstrap {
            file: Some(missing.clone()),
            ..Default::default()
        }
        .resolve(store.clone())
        .is_err());
        let protected = PasswordBootstrap {
            password: Some("saved".into()),
            ..Default::default()
        }
        .resolve(store.clone())
        .unwrap();
        let imported = PasswordBootstrap {
            file: Some(missing.clone()),
            ..Default::default()
        }
        .resolve(store.clone())
        .unwrap();
        assert_eq!(
            imported.snapshot().unwrap().password.as_deref(),
            Some("saved")
        );
        assert!(AuthState::from_sessions(
            imported,
            test_runtime(&store),
            "0.0.0.0".parse().unwrap()
        )
        .is_ok());
        super::super::sessions::reset_password(store.clone()).unwrap();
        let handoff = root.join("old-handoff");
        crate::storage::paths::write_private(&handoff, b"stale-secret").unwrap();
        let disabled = PasswordBootstrap {
            password: Some("stale".into()),
            file: Some(missing),
            restart_file: Some(handoff.clone()),
        }
        .resolve(store.clone())
        .unwrap();
        assert!(!handoff.exists());
        assert!(disabled.snapshot().unwrap().password.is_none());
        store
            .write("webui-password", serde_json::json!({"version":1}))
            .unwrap();
        assert!(PasswordBootstrap {
            password: Some("fallback".into()),
            ..Default::default()
        }
        .resolve(store.clone())
        .is_err());
        drop((disabled, protected, sessions, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn password_body_is_unambiguous_and_current_password_failures_are_throttled() {
        use tower::ServiceExt;
        let (root, store) = store("password-throttle");
        let auth = AuthState::open(store.clone(), Some("synthetic".into())).unwrap();
        let app = app(auth.clone());
        let login = app
            .clone()
            .oneshot(request(
                "/api/login",
                serde_json::json!({"password":"synthetic"}),
                None,
            ))
            .await
            .unwrap();
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        let ambiguous = serde_json::json!({"action":"clear","current_password":"synthetic","new_password":"unwanted"});
        assert_eq!(
            app.clone()
                .oneshot(request("/api/auth/password", ambiguous, Some(&cookie)))
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let revision = auth.sessions.snapshot().unwrap().revision;
        for _ in 0..LOGIN_FAILURE_LIMIT {
            let wrong = serde_json::json!({"action":"clear","current_password":"wrong"});
            assert_eq!(
                app.clone()
                    .oneshot(request("/api/auth/password", wrong, Some(&cookie)))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let correct = serde_json::json!({"action":"clear","current_password":"synthetic"});
        let response = app
            .clone()
            .oneshot(request("/api/auth/password", correct, Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
        assert_eq!(auth.sessions.snapshot().unwrap().revision, revision);
        for path in ["/auth/reset", "/api/auth/reset", "/api/auth/password"] {
            assert!(!is_public_api_path(path));
        }
        drop((app, auth, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn origin_guard_rejects_missing_duplicate_or_malformed_authorities() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:3150".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "http://localhost:3150".parse().unwrap());
        assert!(same_origin(&headers));
        headers.append(header::ORIGIN, "http://localhost:3150".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "http://localhost:3150".parse().unwrap());
        headers.append(header::HOST, "localhost:3150".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::HOST, "localhost:3150".parse().unwrap());
        for origin in [
            " http://localhost:3150",
            "http://localhost:3150#x",
            "http://localhost:3150?x",
            "http://localhost:3150\\",
        ] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(!same_origin(&headers));
        }
        headers.insert(header::HOST, "example.test:443".parse().unwrap());
        headers.insert(header::ORIGIN, "https://example.test".parse().unwrap());
        assert!(same_origin(&headers));
    }

    #[tokio::test]
    async fn browser_sessions_and_peer_credentials_have_separate_authority() {
        use axum::{body::Body, routing::post, Router};
        use tower::ServiceExt;
        let root =
            std::env::temp_dir().join(format!("bilistream-auth-routes-{}", std::process::id()));
        let store =
            crate::storage::Store::open(root.join("data"), root.join("key/master"), None).unwrap();
        let token = "synthetic-cluster-key-0123456789abcdef";
        let auth = AuthState::open(Arc::clone(&store), Some("password".into())).unwrap();
        let router = |auth| {
            Router::new()
                .route("/api/login", post(login))
                .route("/api/logout", post(logout))
                .fallback(|| async { StatusCode::OK })
                .layer(axum::middleware::from_fn(require_webui_auth))
                .layer(Extension(auth))
        };
        let app = router(Arc::clone(&auth));
        let request = |path: &str, cookie: Option<&str>, bearer: Option<&str>| {
            let mut request = Request::builder()
                .method("POST")
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, cookie);
            }
            if let Some(token) = bearer {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            request
                .body(Body::from(r#"{"password":"password"}"#))
                .unwrap()
        };
        let first = app
            .clone()
            .oneshot(request("/api/login", None, None))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_cookie = first.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(first_cookie.contains("HttpOnly; SameSite=Lax; Max-Age="));
        let second = app
            .clone()
            .oneshot(request("/api/login", Some(&first_cookie), None))
            .await
            .unwrap();
        let second_cookie = second.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert_ne!(first_cookie, second_cookie);
        assert_eq!(
            app.clone()
                .oneshot(request("/api/config", Some(&first_cookie), None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/api/config", Some(&second_cookie), None))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    "/api/cluster/heartbeat",
                    Some(&second_cookie),
                    None
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // Legacy shared bearer credentials never authenticate any route.
        for path in [
            "/api/cluster/heartbeat",
            "/api/cluster/drain",
            "/api/config",
            "/api/storage/backup",
            "/api/cluster/unknown",
            "/api/update/download",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(path, None, Some(token)))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            app.clone()
                .oneshot(request("/api/cluster/heartbeat", None, Some("wrong")))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/api/logout", Some(&second_cookie), None))
                .await
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/api/config", Some(&second_cookie), None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // The old deterministic cookie is never accepted, even for the same password.
        use md5::{Digest, Md5};
        let old = format!(
            "bilistream_session={}",
            Md5::digest(b"bilistream-webui-session-v1\0password")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/api/config", Some(&old), None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        super::super::sessions::reset_password(Arc::clone(&store)).unwrap();
        let local = router(AuthState::open(Arc::clone(&store), None).unwrap());
        assert_eq!(
            local
                .clone()
                .oneshot(request("/api/config", None, None))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            local
                .clone()
                .oneshot(request("/api/cluster/heartbeat", None, None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            local
                .clone()
                .oneshot(request("/api/cluster/drain", None, Some("")))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        drop((app, auth, local, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bind_syntax_is_parsed_before_resolved_password_enforcement() {
        for bind in ["0.0.0.0", "::", "192.0.2.1"] {
            assert!(make_listen_config(bind, None).is_ok());
            assert!(make_listen_config(bind, Some("  ".into())).is_ok());
            assert!(make_listen_config(bind, Some("test-password".into())).is_ok());
        }
        for bind in ["127.0.0.1", "::1", "localhost"] {
            assert!(make_listen_config(bind, None).is_ok());
        }
    }

    #[test]
    fn failed_login_limits_are_per_source_expire_and_reset_after_success() {
        let now = Instant::now();
        let first: IpAddr = "192.0.2.1".parse().unwrap();
        let second: IpAddr = "192.0.2.2".parse().unwrap();
        let mut limiter = LoginThrottle::default();
        for _ in 0..LOGIN_FAILURE_LIMIT {
            assert_eq!(limiter.attempt(first, false, now), Ok(false));
        }
        assert!(limiter.attempt(first, true, now).is_err());
        assert_eq!(limiter.attempt(second, true, now), Ok(true));
        assert_eq!(limiter.attempt(first, false, now + LOGIN_WINDOW), Ok(false));
        assert_eq!(limiter.attempt(first, true, now + LOGIN_WINDOW), Ok(true));
        assert!(limiter.failures.is_empty());
    }

    #[test]
    fn login_source_tracking_is_bounded() {
        let now = Instant::now();
        let mut limiter = LoginThrottle::default();
        for i in 0..LOGIN_SOURCE_LIMIT {
            let source = IpAddr::V4(Ipv4Addr::from((i + 1) as u32));
            assert_eq!(limiter.attempt(source, false, now), Ok(false));
        }
        assert!(limiter.attempt(DEFAULT_BIND, false, now).is_err());
        assert_eq!(limiter.failures.len(), LOGIN_SOURCE_LIMIT);
        assert_eq!(
            limiter.attempt(DEFAULT_BIND, false, now + LOGIN_WINDOW),
            Ok(false)
        );
        assert_eq!(limiter.failures.len(), 1);
    }

    #[test]
    fn parse_bind_accepts_localhost_alias() {
        assert_eq!(
            parse_bind("localhost").unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            parse_bind("127.0.0.1").unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn parse_bind_rejects_empty_and_garbage() {
        assert!(parse_bind("").is_err());
        assert!(parse_bind("not-an-ip").is_err());
    }

    #[test]
    fn health_and_login_paths_are_public() {
        assert!(is_public_api_path("/health"));
        assert!(is_public_api_path("/api/health"));
        assert!(is_public_api_path("/auth"));
        assert!(is_public_api_path("/login"));
        assert!(is_public_api_path("/logout"));
        assert!(!is_public_api_path("/status"));
        assert!(!is_public_api_path("/api/status"));
        assert!(!is_public_api_path("/events"));
    }

    #[test]
    fn cookie_value_reads_named_pair() {
        assert_eq!(
            cookie_value(
                "theme=dark; bilistream_session=abc123; other=1",
                COOKIE_NAME
            )
            .as_deref(),
            Some("abc123")
        );
        assert_eq!(cookie_value("theme=dark", COOKIE_NAME), None);
    }

    #[test]
    fn secret_eq_rejects_mismatched_values() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "ab"));
    }
}
