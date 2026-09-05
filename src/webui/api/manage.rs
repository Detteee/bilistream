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

pub(crate) async fn mutate_managed_json<T, F>(file_name: &str, edit: F) -> Result<(), String>
where
    T: DeserializeOwned + Serialize + Send + 'static,
    F: FnOnce(&mut T) -> Result<(), String> + Send + 'static,
{
    crate::config::mutate_json_file(managed_json_path(file_name)?, edit).await
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

// Mutations execute validation, editing, and persistence in one transaction.
pub async fn add_area(Json(payload): Json<AddAreaRequest>) -> Json<ApiResponse<()>> {
    let result = mutate_managed_json("areas.json", move |data: &mut AreasData| {
        if data.areas.iter().any(|area| area.id == payload.id) {
            return Err(format!("Area with ID {} already exists", payload.id));
        }
        data.areas.push(Area {
            id: payload.id,
            name: payload.name,
            title_keywords: payload.title_keywords,
            aliases: payload.aliases,
        });
        data.areas.sort_by_key(|area| area.id);
        Ok(())
    })
    .await;
    managed_mutation_response(result, "分区添加成功")
}

pub async fn get_channels_manage() -> Json<ApiResponse<ChannelsData>> {
    match read_managed_json::<ChannelsData>("channels.json") {
        Ok(channels) => Json(ApiResponse {
            success: true,
            data: Some(channels),
            message: None,
        }),
        Err(error) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(error),
        }),
    }
}

fn managed_mutation_response(result: Result<(), String>, message: &str) -> Json<ApiResponse<()>> {
    match result {
        Ok(()) => managed_json_success(message),
        Err(error) => managed_json_error(error),
    }
}

pub async fn add_channel(Json(payload): Json<AddChannelRequest>) -> Json<ApiResponse<()>> {
    let result = mutate_managed_json("channels.json", move |data: &mut ChannelsData| {
        if payload.platforms.is_empty() {
            return Err("At least one platform (YouTube or Twitch) must be specified".to_string());
        }
        if data
            .channels
            .iter()
            .any(|channel| channel.name == payload.name)
        {
            return Err(format!("Channel '{}' already exists", payload.name));
        }
        data.channels.push(Channel {
            name: payload.name,
            aliases: payload.aliases,
            platforms: payload.platforms,
            riot_puuid: payload.riot_puuid,
        });
        Ok(())
    })
    .await;
    managed_mutation_response(result, "频道添加成功")
}

pub async fn update_channel_manage(
    Json(payload): Json<AddChannelRequest>,
) -> Json<ApiResponse<()>> {
    let result = mutate_managed_json("channels.json", move |data: &mut ChannelsData| {
        if payload.platforms.is_empty() {
            return Err("At least one platform (YouTube or Twitch) must be specified".to_string());
        }
        let channel = data
            .channels
            .iter_mut()
            .find(|channel| channel.name == payload.name)
            .ok_or_else(|| format!("Channel '{}' not found", payload.name))?;
        channel.aliases = payload.aliases;
        channel.platforms = payload.platforms;
        channel.riot_puuid = payload.riot_puuid;
        Ok(())
    })
    .await;
    managed_mutation_response(result, "频道更新成功")
}

pub async fn update_area_manage(Json(payload): Json<AddAreaRequest>) -> Json<ApiResponse<()>> {
    let result = mutate_managed_json("areas.json", move |data: &mut AreasData| {
        let area = data
            .areas
            .iter_mut()
            .find(|area| area.id == payload.id)
            .ok_or_else(|| format!("Area with ID {} not found", payload.id))?;
        area.name = payload.name;
        area.title_keywords = payload.title_keywords;
        area.aliases = payload.aliases;
        Ok(())
    })
    .await;
    managed_mutation_response(result, "分区更新成功")
}

pub async fn delete_area(
    axum::extract::Path(id): axum::extract::Path<u32>,
) -> Json<ApiResponse<()>> {
    let result = mutate_managed_json("areas.json", move |data: &mut AreasData| {
        let previous = data.areas.len();
        data.areas.retain(|area| area.id != id);
        if data.areas.len() == previous {
            return Err(format!("Area with ID {} not found", id));
        }
        Ok(())
    })
    .await;
    managed_mutation_response(result, "分区删除成功")
}

pub async fn delete_channel(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<ApiResponse<()>> {
    let result = mutate_managed_json("channels.json", move |data: &mut ChannelsData| {
        let previous = data.channels.len();
        data.channels.retain(|channel| channel.name != name);
        if data.channels.len() == previous {
            return Err(format!("Channel '{}' not found", name));
        }
        Ok(())
    })
    .await;
    managed_mutation_response(result, "频道删除成功")
}
