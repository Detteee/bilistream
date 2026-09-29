use super::http::{pooled_client, response_json_limited};
use crate::config::load_config;
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Refresh Holodex JWT when within this many seconds of `exp`.
pub const HOLODEX_JWT_REFRESH_BEFORE_EXPIRY_SECS: u64 = 3600 * 24 * 30;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HolodexStream {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub stream_type: String,
    pub topic_id: Option<String>,
    pub published_at: Option<String>,
    pub available_at: Option<String>,
    pub status: String,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub live_viewers: Option<i32>,
    #[serde(default)]
    pub channel: HolodexChannel,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default, rename = "placeholderType")]
    pub placeholder_type: Option<String>,
    /// Set when YouTube `videos.list` classified this row, so its status is
    /// trusted over Holodex's stale-schedule heuristics.
    #[serde(skip)]
    pub yt_confirmed: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct HolodexFavoriteChannel {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct HolodexChannel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub photo: Option<String>,
}

pub async fn get_holodex_streams(
    channel_ids: Vec<String>,
    include_placeholder: bool,
) -> Result<Vec<HolodexStream>, Box<dyn Error>> {
    let cfg = load_config().await?;
    let api_key = match cfg.holodex_api_key {
        Some(key) if !key.is_empty() => key,
        _ => return Err("Holodex API key not configured".into()),
    };

    if channel_ids.is_empty() {
        return Err("No channel IDs provided".into());
    }

    let channels_param = channel_ids.join(",");
    let url = if include_placeholder {
        format!(
            "https://holodex.net/api/v2/users/live?channels={channels_param}&includePlaceholder=true"
        )
    } else {
        format!("https://holodex.net/api/v2/users/live?channels={channels_param}")
    };

    let client = pooled_client(None)?;
    let response = client
        .get(&url)
        .timeout(Duration::from_secs(15))
        .header("X-APIKEY", api_key)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Holodex API error: {}", response.status()).into());
    }

    Ok(response_json_limited(response).await?)
}

fn holodex_get(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    jwt: &str,
) -> reqwest::RequestBuilder {
    let mut request = client
        .get(url)
        .timeout(Duration::from_secs(20))
        .header("User-Agent", "bilistream/1.0");
    for (name, value) in [
        ("X-APIKEY", api_key.to_owned()),
        (
            "Authorization",
            format!("BEARER {}", normalize_holodex_jwt(jwt)),
        ),
    ] {
        request = match reqwest::header::HeaderValue::from_str(&value) {
            Ok(mut header) => {
                header.set_sensitive(true);
                request.header(name, header)
            }
            Err(_) => request.header(name, value), // Let reqwest reject invalid header input.
        };
    }
    request
}

pub fn normalize_holodex_jwt(jwt: &str) -> &str {
    let jwt = jwt.trim();
    match jwt.split_once(char::is_whitespace) {
        Some((prefix, token)) if prefix.eq_ignore_ascii_case("bearer") => token.trim(),
        _ => jwt,
    }
}

/// Complete account roster, including channels with no live/upcoming videos.
/// This request neither refreshes credentials nor writes configuration.
pub async fn get_holodex_favorite_channels(
    api_key: &str,
    jwt: &str,
) -> Result<Vec<HolodexFavoriteChannel>, String> {
    let client = pooled_client(None).map_err(|_| "无法创建 Holodex 连接")?;
    favorite_channels_from(
        &client,
        "https://holodex.net/api/v2/users/favorites",
        api_key,
        jwt,
    )
    .await
}

async fn favorite_channels_from(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    jwt: &str,
) -> Result<Vec<HolodexFavoriteChannel>, String> {
    let jwt = normalize_holodex_jwt(jwt);
    if api_key.trim().is_empty() || jwt.is_empty() {
        return Err("请填写 Holodex API Key 和 JWT，或跳过收藏导入".into());
    }
    let response = holodex_get(client, url, api_key.trim(), jwt)
        .send()
        .await
        .map_err(|_| "无法连接 Holodex，请检查网络后重试，或跳过收藏导入")?;
    match response.status().as_u16() {
        200..=299 => {}
        401 | 403 => {
            return Err("Holodex 凭据无效或已失效，请检查 API Key 并重新登录获取 JWT".into())
        }
        429 => return Err("Holodex 请求过于频繁，请稍后重试".into()),
        status => {
            return Err(format!(
                "Holodex 暂不可用（HTTP {status}），可稍后重试或手动添加"
            ))
        }
    }
    response_json_limited(response)
        .await
        .map_err(|_| "Holodex 收藏响应无效或过大，请稍后重试".into())
}

/// Live/upcoming streams for Holodex account favorites (YouTube + Twitch placeholders).
pub async fn get_holodex_favorites_live(
    api_key: &str,
    jwt: &str,
) -> Result<(HashSet<String>, Vec<HolodexStream>), Box<dyn Error>> {
    let client = pooled_client(None)?;

    let favorites = get_holodex_favorite_channels(api_key, jwt).await?;
    let fav_ids: HashSet<String> = favorites.into_iter().map(|c| c.id).collect();

    let live_resp = holodex_get(
        &client,
        "https://holodex.net/api/v2/users/live?includePlaceholder=true",
        api_key,
        jwt,
    )
    .send()
    .await?;

    if !live_resp.status().is_success() {
        return Err(format!("Holodex favorites live error: {}", live_resp.status()).into());
    }

    let streams: Vec<HolodexStream> = response_json_limited(live_resp).await?;
    let filtered = streams
        .into_iter()
        .filter(|s| fav_ids.contains(&s.channel.id))
        .collect();

    Ok((fav_ids, filtered))
}

pub struct HolodexJwtRefresh {
    pub username: Option<String>,
    pub jwt: Option<String>,
}

pub async fn refresh_holodex_jwt(
    api_key: &str,
    jwt: &str,
) -> Result<Option<HolodexJwtRefresh>, String> {
    let client = pooled_client(None).map_err(|e| e.to_string())?;

    let response = holodex_get(
        &client,
        "https://holodex.net/api/v2/user/refresh",
        api_key,
        jwt,
    )
    .timeout(Duration::from_secs(10))
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = response_json_limited(response)
        .await
        .map_err(|e| e.to_string())?;
    let username = body
        .get("user")
        .and_then(|u| u.get("username"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());
    let jwt = body
        .get("jwt")
        .and_then(|j| j.as_str())
        .filter(|token| !token.is_empty())
        .map(|s| s.to_string());

    if username.is_some() || jwt.is_some() {
        Ok(Some(HolodexJwtRefresh { username, jwt }))
    } else {
        Ok(None)
    }
}

pub fn holodex_unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn holodex_jwt_exp(jwt: &str) -> Option<u64> {
    let payload_b64 = jwt.split('.').nth(1)?;
    let mut padded = payload_b64.to_string();
    let rem = padded.len() % 4;
    if rem != 0 {
        padded.push_str(&"=".repeat(4 - rem));
    }
    let bytes = URL_SAFE.decode(padded.as_bytes()).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(|exp| exp.as_u64())
}

pub fn holodex_jwt_is_expired(jwt: &str) -> bool {
    holodex_jwt_exp(jwt).is_some_and(|exp| holodex_unix_now() >= exp)
}

/// True when `now > exp - 30 days` (JWT is inside the renewal window).
pub fn holodex_jwt_should_refresh(jwt: &str) -> bool {
    match holodex_jwt_exp(jwt) {
        Some(exp) => holodex_unix_now() + HOLODEX_JWT_REFRESH_BEFORE_EXPIRY_SECS > exp,
        None => true,
    }
}

pub struct HolodexJwtSyncResult {
    pub jwt: String,
    /// True when Holodex returned a different JWT string.
    pub token_rotated: bool,
    pub username: Option<String>,
    pub refreshed_at: Option<u64>,
}

/// Calls Holodex `/user/refresh` when `now > exp - 30 days`, or when username is unknown.
pub async fn sync_holodex_jwt_if_needed(
    api_key: &str,
    jwt: &str,
    last_refreshed_at: Option<u64>,
    cached_username: Option<String>,
) -> Result<HolodexJwtSyncResult, String> {
    if !holodex_jwt_should_refresh(jwt) && cached_username.is_some() {
        return Ok(HolodexJwtSyncResult {
            jwt: jwt.to_string(),
            token_rotated: false,
            username: cached_username,
            refreshed_at: last_refreshed_at,
        });
    }

    match refresh_holodex_jwt(api_key, jwt).await? {
        Some(refresh) => {
            let new_jwt = refresh
                .jwt
                .filter(|token| !token.is_empty())
                .unwrap_or_else(|| jwt.to_string());
            Ok(HolodexJwtSyncResult {
                token_rotated: new_jwt != jwt,
                jwt: new_jwt,
                username: refresh.username.or(cached_username),
                refreshed_at: Some(holodex_unix_now()),
            })
        }
        None if holodex_jwt_is_expired(jwt) => Err("Holodex JWT expired and refresh failed".into()),
        None => Ok(HolodexJwtSyncResult {
            jwt: jwt.to_string(),
            token_rotated: false,
            username: cached_username,
            refreshed_at: last_refreshed_at,
        }),
    }
}

pub async fn get_holodex_live_title(
    api_key: &str,
    channel_id: &str,
    channel_name: Option<&str>,
) -> Result<Option<String>, Box<dyn Error>> {
    let client = pooled_client(None)?;
    let url = format!("https://holodex.net/api/v2/users/live?channels={channel_id}");

    let response = client
        .get(&url)
        .timeout(Duration::from_secs(15))
        .header("X-APIKEY", api_key)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("Holodex API error: {}", response.status()).into());
    }

    let videos: Vec<HolodexStream> = response_json_limited(response).await?;
    for video in videos.iter().rev() {
        if !video
            .channel
            .name
            .replace(' ', "")
            .contains(channel_name.unwrap_or(""))
        {
            continue;
        }

        if video
            .topic_id
            .as_deref()
            .is_some_and(|topic| topic.contains("membersonly"))
        {
            continue;
        }

        return Ok(Some(video.title.clone()));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn favorite_roster_includes_offline_channels_and_reports_provider_failures() {
        use axum::http::{HeaderMap, StatusCode, Uri};
        use axum::{routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        crate::install_crypto_provider();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let app = Router::new().fallback(get(move |uri: Uri, headers: HeaderMap| {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                assert_eq!(headers["authorization"], "BEARER test-jwt");
                assert_eq!(headers["x-apikey"], "test-key");
                match uri.path() {
                    "/favorites" => (StatusCode::OK, r#"[{"id":"UCabcdefghijklmnopqrstuv","name":"Offline favorite","live":false,"private":"omit"},{"id":"UC1234567890123456789012"}]"#),
                    "/empty" => (StatusCode::OK, "[]"),
                    "/invalid" => (StatusCode::OK, "not JSON"),
                    "/401" => (StatusCode::UNAUTHORIZED, "private upstream detail"),
                    "/403" => (StatusCode::FORBIDDEN, "private upstream detail"),
                    "/429" => (StatusCode::TOO_MANY_REQUESTS, "private upstream detail"),
                    _ => (StatusCode::INTERNAL_SERVER_ERROR, "private upstream detail"),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = |path| format!("http://{address}/{path}");
        let channels = favorite_channels_from(
            &client,
            &url("favorites"),
            " test-key ",
            " bEaReR test-jwt ",
        )
        .await
        .unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "Offline favorite");
        assert!(channels[1].name.is_empty());
        assert_eq!(
            serde_json::to_value(&channels[0]).unwrap(),
            serde_json::json!({
                "id": "UCabcdefghijklmnopqrstuv", "name": "Offline favorite"
            })
        );
        assert!(
            favorite_channels_from(&client, &url("empty"), "test-key", "test-jwt")
                .await
                .unwrap()
                .is_empty()
        );
        for (path, expected) in [
            ("invalid", "响应无效"),
            ("401", "凭据无效"),
            ("403", "凭据无效"),
            ("429", "过于频繁"),
            ("500", "HTTP 500"),
        ] {
            let error = favorite_channels_from(&client, &url(path), "test-key", "test-jwt")
                .await
                .unwrap_err();
            assert!(error.contains(expected), "{path}: {error}");
            assert!(!error.contains("private upstream detail"));
        }
        assert!(
            favorite_channels_from(&client, &url("favorites"), "", "test-jwt")
                .await
                .is_err()
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            7,
            "one GET per roster request; no refresh or live lookup"
        );
        server.abort();
        let _ = server.await;
    }
}
