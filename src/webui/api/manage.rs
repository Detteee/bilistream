use super::*;

// Data structures for area and channel management
#[derive(Serialize, Deserialize, Debug)]
pub struct Area {
    pub id: u32,
    pub name: String,
    pub title_keywords: Vec<String>,
    pub aliases: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AreasData {
    #[serde(default)]
    pub banned_keywords: Vec<String>,
    #[serde(default)]
    pub streaming_banned_keywords: Vec<String>,
    pub areas: Vec<Area>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Channel {
    pub name: String,
    pub aliases: Vec<String>,
    pub platforms: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub riot_puuid: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChannelsData {
    pub channels: Vec<Channel>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AddAreaRequest {
    pub id: u32,
    pub name: String,
    pub title_keywords: Vec<String>,
    pub aliases: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AddChannelRequest {
    pub name: String,
    pub aliases: Vec<String>,
    pub platforms: HashMap<String, String>,
    pub riot_puuid: Option<String>,
}

pub(crate) fn managed_json_path(file_name: &str) -> Result<PathBuf, String> {
    std::env::current_exe()
        .map(|path| path.with_file_name(file_name))
        .map_err(|e| format!("Failed to resolve {} path: {}", file_name, e))
}

pub(crate) fn read_managed_json<T: DeserializeOwned>(file_name: &str) -> Result<T, String> {
    let path = managed_json_path(file_name)?;
    let data = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", file_name, e))?;
    serde_json::from_str::<T>(&data).map_err(|e| format!("Failed to parse {}: {}", file_name, e))
}

pub(crate) fn write_managed_json<T: Serialize>(file_name: &str, data: &T) -> Result<(), String> {
    let json_str = serde_json::to_string_pretty(data)
        .map_err(|e| format!("Failed to serialize {} data: {}", file_name, e))?;
    let path = managed_json_path(file_name)?;
    std::fs::write(path, json_str).map_err(|e| format!("Failed to write {}: {}", file_name, e))
}

pub(crate) fn managed_json_error(message: String) -> Json<ApiResponse<()>> {
    Json(ApiResponse {
        success: false,
        data: None,
        message: Some(message),
    })
}

pub(crate) fn managed_json_success(success_message: &str) -> Json<ApiResponse<()>> {
    set_config_updated();
    Json(ApiResponse {
        success: true,
        data: Some(()),
        message: Some(success_message.to_string()),
    })
}

// Get all areas
pub async fn get_areas_manage() -> Json<ApiResponse<AreasData>> {
    match read_managed_json::<AreasData>("areas.json") {
        Ok(areas) => Json(ApiResponse {
            success: true,
            data: Some(areas),
            message: None,
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(e),
        }),
    }
}

// Add new area
pub async fn add_area(Json(payload): Json<AddAreaRequest>) -> Json<ApiResponse<()>> {
    // Read current areas
    let mut areas_data = match read_managed_json::<AreasData>("areas.json") {
        Ok(areas) => areas,
        Err(e) => return managed_json_error(e),
    };

    // Check if area ID already exists
    if areas_data.areas.iter().any(|a| a.id == payload.id) {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Area with ID {} already exists", payload.id)),
        });
    }

    // Add new area
    areas_data.areas.push(Area {
        id: payload.id,
        name: payload.name,
        title_keywords: payload.title_keywords,
        aliases: payload.aliases,
    });

    // Sort areas by ID
    areas_data.areas.sort_by_key(|a| a.id);

    if let Err(e) = write_managed_json("areas.json", &areas_data) {
        return managed_json_error(e);
    }
    managed_json_success("分区添加成功")
}

// Get all channels
pub async fn get_channels_manage() -> Json<ApiResponse<ChannelsData>> {
    match read_managed_json::<ChannelsData>("channels.json") {
        Ok(channels) => Json(ApiResponse {
            success: true,
            data: Some(channels),
            message: None,
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(e),
        }),
    }
}

// Add new channel
pub async fn add_channel(Json(payload): Json<AddChannelRequest>) -> Json<ApiResponse<()>> {
    // Validate that at least one platform is provided
    if payload.platforms.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(
                "At least one platform (YouTube or Twitch) must be specified".to_string(),
            ),
        });
    }

    // Read current channels
    let mut channels_data = match read_managed_json::<ChannelsData>("channels.json") {
        Ok(channels) => channels,
        Err(e) => return managed_json_error(e),
    };

    // Check if channel name already exists
    if channels_data
        .channels
        .iter()
        .any(|c| c.name == payload.name)
    {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Channel '{}' already exists", payload.name)),
        });
    }

    // Add new channel
    channels_data.channels.push(Channel {
        name: payload.name,
        aliases: payload.aliases,
        platforms: payload.platforms,
        riot_puuid: payload.riot_puuid,
    });

    if let Err(e) = write_managed_json("channels.json", &channels_data) {
        return managed_json_error(e);
    }
    managed_json_success("频道添加成功")
}

// Update existing channel
pub async fn update_channel_manage(
    Json(payload): Json<AddChannelRequest>,
) -> Json<ApiResponse<()>> {
    // Validate that at least one platform is provided
    if payload.platforms.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(
                "At least one platform (YouTube or Twitch) must be specified".to_string(),
            ),
        });
    }

    // Read current channels
    let mut channels_data = match read_managed_json::<ChannelsData>("channels.json") {
        Ok(channels) => channels,
        Err(e) => return managed_json_error(e),
    };

    // Find and update the channel
    if let Some(channel) = channels_data
        .channels
        .iter_mut()
        .find(|c| c.name == payload.name)
    {
        channel.aliases = payload.aliases;
        channel.platforms = payload.platforms;
        channel.riot_puuid = payload.riot_puuid;

        if let Err(e) = write_managed_json("channels.json", &channels_data) {
            return managed_json_error(e);
        }
        managed_json_success("频道更新成功")
    } else {
        Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Channel '{}' not found", payload.name)),
        })
    }
}

// Update existing area
pub async fn update_area_manage(Json(payload): Json<AddAreaRequest>) -> Json<ApiResponse<()>> {
    // Read current areas
    let mut areas_data = match read_managed_json::<AreasData>("areas.json") {
        Ok(areas) => areas,
        Err(e) => return managed_json_error(e),
    };

    // Find and update the area
    if let Some(area) = areas_data.areas.iter_mut().find(|a| a.id == payload.id) {
        area.name = payload.name;
        area.title_keywords = payload.title_keywords;
        area.aliases = payload.aliases;

        if let Err(e) = write_managed_json("areas.json", &areas_data) {
            return managed_json_error(e);
        }
        managed_json_success("分区更新成功")
    } else {
        Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Area with ID {} not found", payload.id)),
        })
    }
}

// Delete area by ID
pub async fn delete_area(
    axum::extract::Path(id): axum::extract::Path<u32>,
) -> Json<ApiResponse<()>> {
    // Read current areas
    let mut areas_data = match read_managed_json::<AreasData>("areas.json") {
        Ok(areas) => areas,
        Err(e) => return managed_json_error(e),
    };

    // Find and remove the area
    let initial_len = areas_data.areas.len();
    areas_data.areas.retain(|area| area.id != id);

    if areas_data.areas.len() == initial_len {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Area with ID {} not found", id)),
        });
    }

    if let Err(e) = write_managed_json("areas.json", &areas_data) {
        return managed_json_error(e);
    }
    managed_json_success("分区删除成功")
}

// Delete channel by name
pub async fn delete_channel(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<ApiResponse<()>> {
    // Read current channels
    let mut channels_data = match read_managed_json::<ChannelsData>("channels.json") {
        Ok(channels) => channels,
        Err(e) => return managed_json_error(e),
    };

    // Find and remove the channel
    let initial_len = channels_data.channels.len();
    channels_data
        .channels
        .retain(|channel| channel.name != name);

    if channels_data.channels.len() == initial_len {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Channel '{}' not found", name)),
        });
    }

    if let Err(e) = write_managed_json("channels.json", &channels_data) {
        return managed_json_error(e);
    }
    managed_json_success("频道删除成功")
}
