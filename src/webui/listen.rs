use super::sessions::Sessions;
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
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
static CLUSTER_TOKEN: OnceLock<String> = OnceLock::new();

static LISTEN: OnceLock<ListenConfig> = OnceLock::new();

#[derive(Clone, Debug)]
struct ListenConfig {
    bind: IpAddr,
    password: Option<String>,
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
    let password = password.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    });
    if !bind.is_loopback() && password.is_none() {
        return Err("监听非本机地址时必须设置 Web UI 访问密码".into());
    }
    Ok(ListenConfig { bind, password })
}

pub fn listen_bind() -> IpAddr {
    LISTEN
        .get()
        .map(|config| config.bind)
        .unwrap_or(DEFAULT_BIND)
}

/// True when the process was started with a Web UI password.
pub fn password_required() -> bool {
    listen_password().is_some()
}

/// Configured Web UI password, if the process was started with one.
pub fn listen_password() -> Option<&'static str> {
    LISTEN.get().and_then(|config| config.password.as_deref())
}

/// Shared cluster credential, independent of the browser password.
pub(crate) fn cluster_token() -> Option<&'static str> {
    CLUSTER_TOKEN.get().map(String::as_str)
}

pub(crate) fn install_cluster_token_file(path: &std::path::Path) -> Result<(), String> {
    let token = super::restart::read_password_file(path)
        .map_err(|e| format!("无法加载节点通信密钥: {e}"))?;
    validate_cluster_token(&token, listen_password())?;
    CLUSTER_TOKEN
        .set(token)
        .map_err(|_| "节点通信密钥已配置".into())
}

fn validate_cluster_token(token: &str, password: Option<&str>) -> Result<(), String> {
    if !(32..=256).contains(&token.len())
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("节点通信密钥需要 32–256 位字母、数字、下划线或连字符".into());
    }
    if password.is_some_and(|p| secret_eq(token, p)) {
        return Err("节点通信密钥不能与 Web UI 密码相同".into());
    }
    Ok(())
}

pub(super) struct AuthState {
    password: Option<String>,
    cluster_token: Option<String>,
    sessions: Sessions,
    throttle: Mutex<LoginThrottle>,
}

impl AuthState {
    pub(super) fn open(
        store: Arc<crate::storage::Store>,
        password: Option<String>,
        cluster_token: Option<String>,
    ) -> std::io::Result<Arc<Self>> {
        let sessions = Sessions::open(store, password.as_deref())?;
        Ok(Arc::new(Self {
            password,
            cluster_token,
            sessions,
            throttle: Mutex::default(),
        }))
    }

    fn has_session(&self, headers: &HeaderMap) -> bool {
        session_from_headers(headers)
            .is_some_and(|token| self.sessions.contains(&token, crate::storage::now()))
    }

    fn allows(&self, method: &Method, path: &str, headers: &HeaderMap) -> bool {
        let path = path.strip_prefix("/api/").unwrap_or(path);
        let path = path.trim_start_matches('/');
        let peer_only = matches!(
            path,
            "cluster/export-config"
                | "cluster/yt-index"
                | "cluster/capabilities"
                | "cluster/heartbeat"
                | "cluster/self-check"
                | "cluster/apply-node-mode"
                | "cluster/sync-membership"
                | "cluster/sync-config"
                | "cluster/cache-active-monitor-state"
                | "cluster/apply-public-status"
        );
        let peer_method = matches!(
            (method.as_str(), path),
            (
                "GET" | "HEAD",
                "cluster/export-config" | "cluster/yt-index" | "cluster/capabilities"
            ) | (
                "POST",
                "cluster/heartbeat"
                    | "cluster/self-check"
                    | "cluster/apply-node-mode"
                    | "cluster/sync-membership"
                    | "cluster/sync-config"
                    | "cluster/cache-active-monitor-state"
                    | "cluster/apply-public-status"
            )
        );
        let peer_control = method == Method::POST
            && matches!(
                path,
                "cluster/drain" | "cluster/auto-failover" | "cluster/failover" | "server/restart"
            );
        if let Some(header) = headers.get(header::AUTHORIZATION) {
            return (peer_method || peer_control)
                && header
                    .to_str()
                    .ok()
                    .and_then(|h| h.strip_prefix("Bearer "))
                    .zip(self.cluster_token.as_deref())
                    .is_some_and(|(got, expected)| secret_eq(got, expected));
        }
        !peer_only && (self.password.is_none() || self.has_session(headers))
    }
}

#[derive(Serialize)]
pub struct AuthStatus {
    pub required: bool,
    pub authenticated: bool,
}

pub(super) async fn auth_status(
    Extension(auth): Extension<Arc<AuthState>>,
    request: Request,
) -> Json<AuthStatus> {
    let required = auth.password.is_some();
    Json(AuthStatus {
        required,
        authenticated: !required || auth.has_session(request.headers()),
    })
}

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
    let Some(expected) = auth.password.as_deref() else {
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
            auth.sessions
                .issue(previous.as_deref(), crate::storage::now())
        })
        .await;
        let token = match session {
            Ok(Ok(token)) => token,
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
        let result = tokio::task::spawn_blocking(move || auth.sessions.revoke(&token)).await;
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
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if is_public_api_path(request.uri().path()) {
        return Ok(next.run(request).await);
    }

    if auth.allows(request.method(), request.uri().path(), request.headers()) {
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

fn session_set_cookie_header(token: &str) -> HeaderValue {
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn browser_sessions_and_peer_credentials_have_separate_authority() {
        use axum::{body::Body, routing::post, Router};
        use tower::ServiceExt;
        let root =
            std::env::temp_dir().join(format!("bilistream-auth-routes-{}", std::process::id()));
        let store =
            crate::storage::Store::open(root.join("data"), root.join("key/master"), None).unwrap();
        let token = "synthetic-cluster-key-0123456789abcdef";
        let auth = AuthState::open(
            Arc::clone(&store),
            Some("password".into()),
            Some(token.into()),
        )
        .unwrap();
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
        assert_eq!(
            app.clone()
                .oneshot(request("/api/cluster/heartbeat", None, Some(token)))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        for path in [
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
        let local = router(AuthState::open(Arc::clone(&store), None, None).unwrap());
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
    fn node_tokens_are_distinct_valid_header_credentials() {
        let token = "synthetic-cluster-key-0123456789abcdef";
        assert!(validate_cluster_token(token, Some("different-password")).is_ok());
        assert!(validate_cluster_token(token, Some(token)).is_err());
        for bad in [
            "",
            "too-short",
            "synthetic-cluster-key-0123456789abcdef\r\n",
            "synthetic cluster-key-0123456789abcdef",
        ] {
            assert!(validate_cluster_token(bad, None).is_err());
        }
    }

    #[test]
    fn non_loopback_listeners_require_a_password() {
        for bind in ["0.0.0.0", "::", "192.0.2.1"] {
            assert!(make_listen_config(bind, None).is_err());
            assert!(make_listen_config(bind, Some("  ".into())).is_err());
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
