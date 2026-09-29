//! First-run channel roster and official area selection. Existing rules survive.
use super::*;
use crate::plugins::holodex::{get_holodex_favorite_channels, HolodexFavoriteChannel};

const AREA_CATALOG_URL: &str = "https://api.live.bilibili.com/room/v1/Area/getList";

#[derive(Deserialize)]
pub struct SetupFavoritesRequest {
    api_key: String,
    jwt: String,
}

pub async fn preview_setup_favorites(
    Json(payload): Json<SetupFavoritesRequest>,
) -> Json<ApiResponse<Vec<HolodexFavoriteChannel>>> {
    let jwt = crate::plugins::holodex::normalize_holodex_jwt(&payload.jwt);
    if crate::plugins::holodex::holodex_jwt_is_expired(jwt) {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some("Holodex JWT 已过期，请重新登录 Holodex 后复制 JWT".into()),
        });
    }
    match get_holodex_favorite_channels(&payload.api_key, &payload.jwt).await {
        Ok(channels) => {
            let mut seen = HashSet::new();
            let channels: Vec<_> = channels
                .into_iter()
                .filter(|channel| {
                    crate::plugins::youtube_channel::is_channel_id(&channel.id)
                        && seen.insert(channel.id.clone())
                })
                .collect();
            Json(ApiResponse {
                success: true,
                message: channels
                    .is_empty()
                    .then(|| "收藏夹中没有可导入的 YouTube 频道，可跳过或手动添加".into()),
                data: Some(channels),
            })
        }
        Err(message) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(message),
        }),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SetupArea {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub parent_name: String,
}

/// Normalize the upstream's string IDs once, at the HTTP boundary.
fn parse_area_catalog(value: serde_json::Value) -> Result<Vec<SetupArea>, String> {
    if value.get("code").and_then(|v| v.as_i64()) != Some(0) {
        return Err("Bilibili 分区列表暂不可用".into());
    }
    let groups = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or("分区列表格式无效")?;
    let mut areas = Vec::new();
    let mut seen = HashSet::new();
    for group in groups {
        let parent = group
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        for item in group
            .get("list")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let id = item
                .get("id")
                .and_then(|id| id.as_u64().or_else(|| id.as_str()?.parse().ok()))
                .and_then(|id| u32::try_from(id).ok())
                .filter(|id| *id > 0);
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let (Some(id), Some(name)) = (id, name) {
                if seen.insert(id) {
                    areas.push(SetupArea {
                        id,
                        name: name.into(),
                        parent_name: parent.into(),
                    });
                }
            }
        }
    }
    if areas.is_empty() {
        return Err("Bilibili 未返回可用分区，请使用本地分区或稍后重试".into());
    }
    Ok(areas)
}

pub async fn get_area_catalog() -> Json<ApiResponse<Vec<SetupArea>>> {
    let result = async {
        let client = crate::plugins::http::pooled_client(None).map_err(|e| e.to_string())?;
        let response = client
            .get(AREA_CATALOG_URL)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("分区列表返回 HTTP {}", response.status()));
        }
        let bytes = crate::plugins::http::response_bytes_limited(response, 2 * 1024 * 1024)
            .await
            .map_err(|e| e.to_string())?;
        parse_area_catalog(
            serde_json::from_slice(&bytes).map_err(|e| format!("分区列表格式无效: {e}"))?,
        )
    }
    .await;
    match result {
        Ok(data) => Json(ApiResponse {
            success: true,
            data: Some(data),
            message: None,
        }),
        Err(error) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(error),
        }),
    }
}

#[derive(Clone, Debug)]
pub(super) struct SetupChannel {
    pub name: String,
    pub platform: &'static str,
    pub id: String,
}

pub(super) fn setup_channel(
    platform: &'static str,
    name: Option<&str>,
    id: Option<&str>,
) -> Result<Option<SetupChannel>, String> {
    let id = id.unwrap_or_default().trim();
    let name = name.unwrap_or_default().trim();
    if id.is_empty() && name.is_empty() {
        return Ok(None);
    }
    if id.is_empty() {
        return Err(format!("{platform} 请输入频道 ID"));
    }
    let id = match platform {
        "youtube" if crate::plugins::youtube_channel::is_channel_id(id) => id.to_string(),
        "twitch"
            if id.len() <= 25 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
        {
            id.to_ascii_lowercase()
        }
        "niconico" => {
            let normalized = crate::plugins::normalize_channel_id(id);
            if normalized.is_empty()
                || !normalized
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(
                    "Niconico 请输入频道 ID 或 ch.nicovideo.jp 频道地址，不能使用 lv 放送 ID"
                        .into(),
                );
            }
            normalized
        }
        "youtube" => {
            return Err("YouTube 请输入 UC 开头的 24 位频道 ID，不是视频 ID 或 @handle".into())
        }
        _ => return Err("Twitch 请输入账号登录名，不是数字 ID 或网址".into()),
    };
    Ok(Some(SetupChannel {
        name: if name.is_empty() {
            id.clone()
        } else {
            name.into()
        },
        platform,
        id,
    }))
}

fn merge_channels(data: &mut serde_json::Value, channels: &[SetupChannel]) -> Result<(), String> {
    let rows = data
        .get_mut("channels")
        .and_then(|v| v.as_array_mut())
        .ok_or("channels.json 格式无效")?;
    for channel in channels {
        // Provider names can change; the platform ID identifies an existing row.
        // Keep all existing display names, aliases and other platform mappings.
        if rows.iter().any(|row| {
            row.get("platforms")
                .and_then(|p| p.get(channel.platform))
                .and_then(|id| id.as_str())
                == Some(&channel.id)
        }) {
            continue;
        }
        if let Some(row) = rows
            .iter_mut()
            .find(|row| row.get("name").and_then(|v| v.as_str()) == Some(&channel.name))
        {
            let platforms = row
                .get_mut("platforms")
                .and_then(|v| v.as_object_mut())
                .ok_or("已有频道的平台配置格式无效")?;
            if platforms
                .get(channel.platform)
                .is_some_and(|id| id.as_str() != Some(&channel.id))
            {
                return Err(format!(
                    "频道 {} 已有不同的 {} ID，请在配置管理中修改",
                    channel.name, channel.platform
                ));
            }
            platforms.insert(channel.platform.into(), json!(channel.id));
        } else {
            rows.push(json!({"name": channel.name, "aliases": [], "platforms": {channel.platform: channel.id}}));
        }
    }
    Ok(())
}

fn merge_areas(data: &mut serde_json::Value, areas: &[SetupArea]) -> Result<(), String> {
    let rows = data
        .get_mut("areas")
        .and_then(|v| v.as_array_mut())
        .ok_or("areas.json 格式无效")?;
    for area in areas {
        if area.id == 0 || area.name.trim().is_empty() {
            return Err("分区 ID 和名称不能为空".into());
        }
        if !rows
            .iter()
            .any(|row| row.get("id").and_then(|v| v.as_u64()) == Some(u64::from(area.id)))
        {
            rows.push(json!({"id": area.id, "name": area.name.trim(), "title_keywords": [], "aliases": []}));
        }
    }
    Ok(())
}

pub(super) async fn save_setup_lists(
    channels: Vec<SetupChannel>,
    areas: Vec<SetupArea>,
) -> Result<(), String> {
    let path = managed_json_path("channels.json")?;
    let directory = path.parent().ok_or("无法定位配置目录")?;
    crate::deps::initialize_user_data(directory).await?;
    // Validate both edits before writing either file. Transactions revalidate
    // their latest contents; retries are additive and idempotent.
    let mut current_channels: serde_json::Value = read_managed_json("channels.json")?;
    let mut current_areas: serde_json::Value = read_managed_json("areas.json")?;
    merge_channels(&mut current_channels, &channels)?;
    merge_areas(&mut current_areas, &areas)?;
    if !channels.is_empty() {
        mutate_managed_json("channels.json", move |data: &mut serde_json::Value| {
            merge_channels(data, &channels)
        })
        .await?;
    }
    if !areas.is_empty() {
        mutate_managed_json("areas.json", move |data: &mut serde_json::Value| {
            merge_areas(data, &areas)
        })
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_favorite_preview_fails_before_any_provider_request() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let jwt = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#)
        );
        let Json(response) = preview_setup_favorites(Json(SetupFavoritesRequest {
            api_key: "synthetic-key".into(),
            jwt: format!("Bearer {jwt}"),
        }))
        .await;
        assert!(!response.success);
        assert!(response.data.is_none());
        assert!(response.message.unwrap().contains("已过期"));
    }

    #[test]
    fn catalog_normalizes_ids_and_rejects_failed_or_empty_responses() {
        let data = parse_area_catalog(json!({"code":0,"data":[{"name":"网游","list":[{"id":"329","name":"无畏契约"},{"id":329,"name":"duplicate"},{"id":"bad","name":"bad"}]}]})).unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].id, 329);
        assert_eq!(data[0].parent_name, "网游");
        assert!(parse_area_catalog(json!({"code":-1,"data":[]})).is_err());
        assert!(parse_area_catalog(json!({"code":0,"data":[]})).is_err());
    }

    #[test]
    fn channel_inputs_are_platform_ids_and_merge_without_losing_existing_rules() {
        assert!(setup_channel("youtube", None, Some("@demo")).is_err());
        let yt = setup_channel("youtube", Some("Mine"), Some("UCabcdefghijklmnopqrstuv"))
            .unwrap()
            .unwrap();
        let tw = setup_channel("twitch", Some("Mine"), Some("Mine_TW"))
            .unwrap()
            .unwrap();
        let nc = setup_channel("niconico", None, Some("https://ch.nicovideo.jp/demo?x=1"))
            .unwrap()
            .unwrap();
        assert_eq!(nc.id, "demo");
        assert!(setup_channel("niconico", None, Some("lv123")).is_err());
        let mut data = json!({"channels":[{"name":"Mine","aliases":["alias"],"platforms":{"youtube":yt.id},"custom":1}],"custom":true});
        merge_channels(&mut data, &[yt, tw]).unwrap();
        assert_eq!(data["channels"][0]["platforms"]["twitch"], "mine_tw");
        assert_eq!(data["channels"][0]["aliases"], json!(["alias"]));
        assert_eq!(data["custom"], true);
        assert!(merge_channels(
            &mut data,
            &[SetupChannel {
                name: "Mine".into(),
                platform: "youtube",
                id: "different".into()
            }]
        )
        .is_err());
    }

    #[test]
    fn selected_areas_append_without_replacing_keywords_or_custom_fields() {
        let mut data = json!({"areas":[{"id":235,"name":"Mine","title_keywords":["keep"]}],"streaming_banned_keywords":["keep"],"custom":1});
        let selected = vec![
            SetupArea {
                id: 235,
                name: "其他单机".into(),
                parent_name: String::new(),
            },
            SetupArea {
                id: 329,
                name: "无畏契约".into(),
                parent_name: String::new(),
            },
        ];
        merge_areas(&mut data, &selected).unwrap();
        merge_areas(&mut data, &selected).unwrap();
        assert_eq!(data["areas"].as_array().unwrap().len(), 2);
        assert_eq!(data["areas"][0]["title_keywords"], json!(["keep"]));
        assert_eq!(data["areas"][1]["aliases"], json!([]));
        assert_eq!(data["custom"], 1);
    }
}

#[derive(Deserialize)]
pub struct ResolveChannelRequest {
    input: String,
}

pub async fn resolve_youtube_channel(
    Json(payload): Json<ResolveChannelRequest>,
) -> Json<ApiResponse<serde_json::Value>> {
    let proxy = load_config().await.ok().and_then(|cfg| cfg.youtube.proxy);
    match crate::plugins::youtube_channel::resolve_channel_id(&payload.input, proxy.as_deref())
        .await
    {
        Ok(id) => Json(ApiResponse {
            success: true,
            data: Some(json!({"channel_id":id})),
            message: None,
        }),
        Err(error) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(error),
        }),
    }
}
