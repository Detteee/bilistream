use crate::config::load_config;
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Refresh Holodex JWT when within this many seconds of `exp`.
pub const HOLODEX_JWT_REFRESH_BEFORE_EXPIRY_SECS: u64 = 3600 * 24 * 30;

#[derive(Serialize, Deserialize, Debug)]
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
}

#[derive(Serialize, Deserialize, Debug)]
pub struct HolodexFavoriteChannel {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Default)]
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client.get(&url).header("X-APIKEY", api_key).send().await?;

    if !response.status().is_success() {
        return Err(format!("Holodex API error: {}", response.status()).into());
    }

    Ok(response.json().await?)
}

fn holodex_get(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    jwt: &str,
) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header("X-APIKEY", api_key)
        .header("Authorization", format!("BEARER {jwt}"))
        .header("User-Agent", "bilistream/1.0")
}

/// Live/upcoming streams for Holodex account favorites (YouTube + Twitch placeholders).
pub async fn get_holodex_favorites_live(
    api_key: &str,
    jwt: &str,
) -> Result<(HashSet<String>, Vec<HolodexStream>), Box<dyn Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    let fav_resp = holodex_get(
        &client,
        "https://holodex.net/api/v2/users/favorites",
        api_key,
        jwt,
    )
    .send()
    .await?;

    if !fav_resp.status().is_success() {
        return Err(format!("Holodex favorites error: {}", fav_resp.status()).into());
    }

    let favorites: Vec<HolodexFavoriteChannel> = fav_resp.json().await?;
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

    let streams: Vec<HolodexStream> = live_resp.json().await?;
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let response = holodex_get(
        &client,
        "https://holodex.net/api/v2/user/refresh",
        api_key,
        jwt,
    )
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let url = format!("https://holodex.net/api/v2/users/live?channels={channel_id}");

    let response = client.get(&url).header("X-APIKEY", api_key).send().await?;
    if !response.status().is_success() {
        return Err(format!("Holodex API error: {}", response.status()).into());
    }

    let videos: Vec<HolodexStream> = response.json().await?;
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
