use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;

const DEFAULT_BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const COOKIE_NAME: &str = "bilistream_session";
const SESSION_PEPPER: &[u8] = b"bilistream-webui-session-v1";

static LISTEN: OnceLock<ListenConfig> = OnceLock::new();

#[derive(Clone, Debug)]
struct ListenConfig {
    bind: IpAddr,
    password: Option<String>,
    session: Option<String>,
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
    let bind = parse_bind(bind)?;
    let password = password.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    });
    let session = password.as_deref().map(session_id_for_password);
    LISTEN
        .set(ListenConfig {
            bind,
            password,
            session,
        })
        .map_err(|_| "Web UI listen address already configured".to_string())
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

/// Session cookie value derived from the password, if one is configured.
pub fn session_cookie() -> Option<&'static str> {
    LISTEN.get().and_then(|config| config.session.as_deref())
}

/// Attach the session cookie when a password is configured.
pub fn authorize_http(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match session_cookie() {
        Some(session) => {
            builder.header(reqwest::header::COOKIE, format!("{COOKIE_NAME}={session}"))
        }
        None => builder,
    }
}

#[derive(Serialize)]
pub struct AuthStatus {
    pub required: bool,
    pub authenticated: bool,
}

pub async fn auth_status(request: Request) -> Json<AuthStatus> {
    let required = password_required();
    Json(AuthStatus {
        required,
        authenticated: !required || request_has_session(&request),
    })
}

#[derive(Deserialize)]
pub struct LoginBody {
    #[serde(default)]
    password: String,
}

pub async fn login(Json(body): Json<LoginBody>) -> Response {
    let Some(expected) = listen_password() else {
        return Json(serde_json::json!({ "success": true, "required": false })).into_response();
    };

    if secret_eq(body.password.trim(), expected) {
        let mut response = Json(serde_json::json!({ "success": true })).into_response();
        if let Some(cookie) = session_set_cookie_header() {
            response.headers_mut().insert(header::SET_COOKIE, cookie);
        }
        response
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "success": false, "message": "密码错误" })),
        )
            .into_response()
    }
}

pub async fn logout() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(cookie) = HeaderValue::from_str(&format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
    )) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

pub async fn require_webui_auth(request: Request, next: Next) -> Result<Response, StatusCode> {
    if is_public_api_path(request.uri().path()) {
        return Ok(next.run(request).await);
    }

    if !password_required() || request_has_session(&request) {
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

fn session_id_for_password(password: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(SESSION_PEPPER);
    hasher.update([0u8]);
    hasher.update(password.as_bytes());
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(32), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn session_set_cookie_header() -> Option<HeaderValue> {
    let session = session_cookie()?;
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"
    ))
    .ok()
}

fn request_has_session(request: &Request) -> bool {
    let Some(expected) = session_cookie() else {
        return true;
    };
    request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME))
        .is_some_and(|got| secret_eq(&got, expected))
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
    fn session_id_is_stable_for_the_same_password() {
        assert_eq!(
            session_id_for_password("hunter2"),
            session_id_for_password("hunter2")
        );
        assert_ne!(
            session_id_for_password("hunter2"),
            session_id_for_password("hunter3")
        );
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
