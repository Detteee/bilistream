//! Read-only session validity warnings; this never renews or replaces a cookie.
use crate::config::Config;
use serde::Serialize;
use std::hash::{Hash, Hasher};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const CHECK_URL: &str = "https://nvapi.nicovideo.jp/v1/users/me";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionState {
    Unconfigured,
    Unchecked,
    Valid,
    Invalid,
    Unavailable,
}

#[derive(Clone, Serialize)]
pub(crate) struct SessionStatus {
    state: SessionState,
    checked_at: Option<String>,
    message: String,
}

impl SessionStatus {
    fn new(state: SessionState, message: &str) -> Self {
        Self {
            state,
            checked_at: None,
            message: message.into(),
        }
    }
}
struct Checked {
    identity: u64,
    at: Instant,
    status: SessionStatus,
}
static CHECKED: Mutex<Option<Checked>> = Mutex::new(None);
static CHECK_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

fn identity(session: &str, proxy: Option<&str>) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    session.hash(&mut hash);
    proxy.hash(&mut hash);
    hash.finish()
}
fn configured(cfg: &Config) -> bool {
    cfg.niconico
        .user_session
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
        || cfg
            .niconico
            .cookies_file
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
}
fn cached(identity: u64) -> Option<(SessionStatus, Duration)> {
    CHECKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|checked| checked.identity == identity)
        .map(|checked| (checked.status.clone(), checked.at.elapsed()))
}

pub(crate) fn status(cfg: &Config) -> SessionStatus {
    if !configured(cfg) {
        return SessionStatus::new(SessionState::Unconfigured, "未配置 user_session");
    }
    let Ok(session) = super::niconico::configured_user_session(&cfg.niconico) else {
        return SessionStatus::new(
            SessionState::Unavailable,
            "无法读取会话，请检查旧 Cookie 文件路径",
        );
    };
    cached(identity(&session, cfg.niconico.proxy.as_deref()))
        .map(|(status, _)| status)
        .unwrap_or_else(|| {
            SessionStatus::new(SessionState::Unchecked, "尚未检查；检查不会延长会话有效期")
        })
}

fn check_interval(state: &SessionState) -> Duration {
    if *state == SessionState::Unavailable {
        RETRY
    } else {
        DAY
    }
}

fn classify(status: reqwest::StatusCode, body: Option<&serde_json::Value>) -> SessionStatus {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        SessionStatus::new(
            SessionState::Invalid,
            "登录会话已失效，请重新登录 Niconico 并更新 user_session",
        )
    } else if status.is_success()
        && body.is_some_and(|value| value["meta"]["status"] == 200 && value["data"].is_object())
    {
        SessionStatus::new(SessionState::Valid, "会话仍被接受；本次检查不会续期")
    } else {
        SessionStatus::new(
            SessionState::Unavailable,
            "暂时无法验证（网络或服务响应异常），不能据此判断会话失效",
        )
    }
}

async fn probe(session: &str, proxy: Option<&str>) -> SessionStatus {
    let result = async {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15));
        if let Some(proxy) = proxy.filter(|s| !s.is_empty()) {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| ())?);
        }
        let client = builder.build().map_err(|_| ())?;
        let mut cookie = reqwest::header::HeaderValue::from_str(&format!("user_session={session}"))
            .map_err(|_| ())?;
        cookie.set_sensitive(true);
        let response = client
            .get(CHECK_URL)
            .header(reqwest::header::COOKIE, cookie)
            .header("X-Frontend-Id", "6")
            .header("X-Frontend-Version", "0")
            .header("X-Niconico-Language", "en-us")
            .send()
            .await
            .map_err(|_| ())?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(classify(status, None));
        }
        let bytes = super::http::response_bytes_limited(response, 256 * 1024)
            .await
            .map_err(|_| ())?;
        let body = serde_json::from_slice(&bytes).ok();
        Ok::<_, ()>(classify(status, body.as_ref()))
    }
    .await;
    result.unwrap_or_else(|_| {
        SessionStatus::new(
            SessionState::Unavailable,
            "暂时无法验证（网络或服务响应异常），稍后重试",
        )
    })
}

pub(crate) async fn check(cfg: &Config, force: bool) -> SessionStatus {
    let _guard = CHECK_LOCK.lock().await;
    if !configured(cfg) {
        return status(cfg);
    }
    let Ok(session) = super::niconico::configured_user_session(&cfg.niconico) else {
        return status(cfg);
    };
    let fingerprint = identity(&session, cfg.niconico.proxy.as_deref());
    let previous = cached(fingerprint);
    if !force {
        if let Some((status, age)) = &previous {
            let interval = check_interval(&status.state);
            if *age < interval {
                return status.clone();
            }
        }
    }
    let mut status = probe(&session, cfg.niconico.proxy.as_deref()).await;
    status.checked_at = Some(chrono::Utc::now().to_rfc3339());
    // An in-flight check for an older credential must not paint a new setting.
    if crate::config::config_is_current(cfg) {
        if previous
            .as_ref()
            .is_none_or(|(old, _)| old.state != status.state)
        {
            if status.state == SessionState::Invalid {
                tracing::warn!("Niconico: {}", status.message);
            } else {
                tracing::info!("Niconico: {}", status.message);
            }
        }
        *CHECKED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Checked {
            identity: fingerprint,
            at: Instant::now(),
            status: status.clone(),
        });
    } else {
        return SessionStatus::new(SessionState::Unchecked, "配置已更改，请重新检查当前会话");
    }
    status
}

pub struct SessionWorker(tokio::task::JoinHandle<()>);
impl Drop for SessionWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub fn start_session_worker() -> SessionWorker {
    SessionWorker(tokio::spawn(async {
        loop {
            let cfg = crate::config::load_config().await.ok();
            if let Some(cfg) = cfg.filter(|cfg| cfg.niconico.session_check_enabled) {
                check(&cfg, false).await;
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daily_checks_do_not_become_keep_alive_and_transient_errors_retry_later() {
        assert_eq!(
            check_interval(&SessionState::Valid),
            Duration::from_secs(86400)
        );
        assert_eq!(
            check_interval(&SessionState::Invalid),
            Duration::from_secs(86400)
        );
        assert_eq!(
            check_interval(&SessionState::Unavailable),
            Duration::from_secs(3600)
        );
        assert_ne!(identity("first", None), identity("second", None));
    }
    #[test]
    fn only_an_auth_rejection_means_expired_and_html_is_not_success() {
        assert_eq!(
            classify(reqwest::StatusCode::UNAUTHORIZED, None).state,
            SessionState::Invalid
        );
        for code in [
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::OK,
        ] {
            assert_eq!(classify(code, None).state, SessionState::Unavailable);
        }
        assert_eq!(
            classify(
                reqwest::StatusCode::OK,
                Some(&serde_json::json!({"meta":{"status":200},"data":{"user":{}}}))
            )
            .state,
            SessionState::Valid
        );
    }
}
