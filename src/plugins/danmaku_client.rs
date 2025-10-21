use anyhow::Result;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use flate2::read::ZlibDecoder;
use futures_util::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use md5::{Digest, Md5};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::plugins::{bili_stop_live, send_danmaku};

// Aknowledgement: Isoheptane/bilibili-live-danmaku-cli
lazy_static! {
    static ref WBI_CACHE_DIR: PathBuf = {
        let exe_path = std::env::current_exe().unwrap();
        let mut path = exe_path;
        path.pop(); // Go up one directory from the executable
        path.join(".wbi_cache")
    };
}

const WBI_CACHE_DURATION: u64 = 12 * 60 * 60; // 12 hours in seconds

// Bilibili danmaku protocol constants
const HEADER_LENGTH: u32 = 16;

// Protocol types (header protocol field)
const PROTOCOL_COMMAND: u16 = 0;
const PROTOCOL_COMMAND_ZLIB: u16 = 2;
const PROTOCOL_COMMAND_BROTLI: u16 = 3;

// Operation codes (packet type)
const OP_HEARTBEAT: u32 = 2;
const OP_HEARTBEAT_REPLY: u32 = 3;
const OP_MESSAGE: u32 = 5;
const OP_AUTH: u32 = 7;
const OP_AUTH_REPLY: u32 = 8;

// Protocol versions (body protover field)
#[allow(dead_code)]
const PROTOVER_NORMAL: u8 = 1;
#[allow(dead_code)]
const PROTOVER_BROTLI: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DanmakuConfig {
    pub room_id: u64,
    pub sessdata: String,
    pub bili_jct: String,
    pub dede_user_id: String,
    pub dede_user_id_ckmd5: String,
    pub buvid3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DanmakuMessage {
    pub cmd: String,
    pub info: Option<Value>,
    pub data: Option<Value>,
}

pub struct BilibiliDanmakuClient {
    config: DanmakuConfig,
    room_id: u64,
    #[allow(dead_code)]
    token: Option<String>, // Kept for potential future use with getDanmuInfo
    host_list: Vec<String>,
    app_config: Arc<Config>,
    enable_commands: Arc<AtomicBool>,
}

impl BilibiliDanmakuClient {
    pub fn new(
        config: DanmakuConfig,
        app_config: Arc<Config>,
        enable_commands: Arc<AtomicBool>,
    ) -> Self {
        Self {
            room_id: config.room_id,
            config,
            token: None,
            host_list: Vec::new(),
            app_config,
            enable_commands,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        // Get danmaku server info and token (like the reference implementation)
        // This is required for proper authentication
        match self.get_danmaku_info().await {
            Ok(_) => {
                // Successfully got server info - suppress log
            }
            Err(e) => {
                warn!("Failed to get danmaku info: {}, using fallback", e);
                // Fallback to hardcoded servers with empty token
                self.host_list = vec![
                    "broadcastlv.chat.bilibili.com".to_string(),
                    "tx-sh-live-comet-04.chat.bilibili.com".to_string(),
                    "tx-bj-live-comet-04.chat.bilibili.com".to_string(),
                ];
                self.token = Some(String::new());
            }
        }

        // Connect to WebSocket
        let ws_url = format!("wss://{}/sub", self.host_list[0]);

        let (ws_stream, _) = connect_async(&ws_url).await?;
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Send authentication
        let auth_packet = self.create_auth_packet()?;
        ws_sender.send(Message::Binary(auth_packet)).await?;

        // Start heartbeat task
        let mut heartbeat_interval = interval(Duration::from_secs(30));
        let heartbeat_packet = self.create_heartbeat_packet();

        loop {
            tokio::select! {
                // Handle incoming messages
                msg = ws_receiver.next() => {
                    match msg {
                        Some(Ok(Message::Binary(data))) => {
                            if let Err(e) = self.handle_message(&data).await {
                                error!("Error handling message: {}", e);
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            warn!("WebSocket connection closed by server");
                            break;
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        None => {
                            warn!("WebSocket stream ended");
                            break;
                        }
                        _ => {}
                    }
                }
                // Send heartbeat
                _ = heartbeat_interval.tick() => {
                    if let Err(e) = ws_sender.send(Message::Binary(heartbeat_packet.clone())).await {
                        error!("Failed to send heartbeat: {}", e);
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    // WBI signature helper functions (same as in bilibili.rs)
    fn gen_mixin_key(raw_wbi_key: &str) -> String {
        const MIXIN_KEY_ENC_TAB: [u8; 64] = [
            46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42,
            19, 29, 28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60,
            51, 30, 4, 22, 25, 54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
        ];

        let raw_bytes = raw_wbi_key.as_bytes();
        let mixin_key: String = MIXIN_KEY_ENC_TAB
            .iter()
            .take(32)
            .map(|&n| raw_bytes[n as usize] as char)
            .collect();
        mixin_key
    }

    fn url_encode(s: &str) -> String {
        utf8_percent_encode(s, NON_ALPHANUMERIC)
            .to_string()
            .replace('+', "%20")
    }

    fn calculate_w_rid(params: &BTreeMap<&str, String>, mixin_key: &str) -> String {
        let encoded_params: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, Self::url_encode(v)))
            .collect();

        let param_string = encoded_params.join("&");
        let string_to_hash = format!("{}{}", param_string, mixin_key);

        let mut hasher = Md5::new();
        hasher.update(string_to_hash.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    async fn get_wbi_keys(&self) -> Result<(String, String)> {
        // Create cache directory if it doesn't exist
        fs::create_dir_all(&*WBI_CACHE_DIR)?;

        let img_key_path = WBI_CACHE_DIR.join("img_key");
        let sub_key_path = WBI_CACHE_DIR.join("sub_key");
        let timestamp_path = WBI_CACHE_DIR.join("timestamp");

        // Check if we have cached keys and if they're still valid
        if img_key_path.exists() && sub_key_path.exists() && timestamp_path.exists() {
            if let Ok(timestamp_str) = fs::read_to_string(&timestamp_path) {
                if let Ok(timestamp) = timestamp_str.parse::<u64>() {
                    let current_time = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    if current_time - timestamp < WBI_CACHE_DURATION {
                        // Cache is still valid, read the keys
                        if let (Ok(img_key), Ok(sub_key)) = (
                            fs::read_to_string(&img_key_path),
                            fs::read_to_string(&sub_key_path),
                        ) {
                            // Using cached WBI keys - suppress log
                            return Ok((img_key.trim().to_string(), sub_key.trim().to_string()));
                        }
                    }
                }
            }
        }

        // Cache miss or expired, fetch new keys
        // info!("Fetching fresh WBI keys from Bilibili API...");
        let client = reqwest::Client::new();
        let response: Value = client
            .get("https://api.bilibili.com/x/web-interface/nav")
            .send()
            .await?
            .json()
            .await?;

        let wbi_img = response["data"]["wbi_img"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Missing wbi_img in nav response"))?;

        let img_url = wbi_img["img_url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing img_url"))?;
        let sub_url = wbi_img["sub_url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing sub_url"))?;

        let img_key = img_url
            .split('/')
            .last()
            .and_then(|s| s.split('.').next())
            .ok_or_else(|| anyhow::anyhow!("Invalid img_url format"))?
            .to_string();
        let sub_key = sub_url
            .split('/')
            .last()
            .and_then(|s| s.split('.').next())
            .ok_or_else(|| anyhow::anyhow!("Invalid sub_url format"))?
            .to_string();

        // Cache the keys
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        fs::write(&img_key_path, &img_key)?;
        fs::write(&sub_key_path, &sub_key)?;
        fs::write(&timestamp_path, current_time.to_string())?;

        // info!("WBI keys cached successfully");

        Ok((img_key, sub_key))
    }

    #[allow(dead_code)]
    async fn get_danmaku_info(&mut self) -> Result<()> {
        // info!("Getting WBI keys for signed request...");
        let (img_key, sub_key) = self.get_wbi_keys().await?;
        let raw_wbi_key = format!("{}{}", img_key, sub_key);
        let mixin_key = Self::gen_mixin_key(&raw_wbi_key);

        // Get current timestamp
        let wts = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .to_string();

        // Build parameters for WBI signature
        let mut params = BTreeMap::new();
        params.insert("id", self.room_id.to_string());
        params.insert("type", "0".to_string());
        params.insert("wts", wts.clone());

        // Calculate w_rid
        let w_rid = Self::calculate_w_rid(&params, &mixin_key);

        // Build query string
        let query_string = format!("id={}&type=0&wts={}&w_rid={}", self.room_id, wts, w_rid);

        // info!("Requesting getDanmuInfo with WBI signature...");

        let client = reqwest::Client::new();
        let cookie = if !self.config.buvid3.is_empty() {
            format!(
                "SESSDATA={}; bili_jct={}; DedeUserID={}; DedeUserID__ckMd5={}; buvid3={}",
                self.config.sessdata,
                self.config.bili_jct,
                self.config.dede_user_id,
                self.config.dede_user_id_ckmd5,
                self.config.buvid3
            )
        } else {
            format!(
                "SESSDATA={}; bili_jct={}; DedeUserID={}; DedeUserID__ckMd5={}",
                self.config.sessdata,
                self.config.bili_jct,
                self.config.dede_user_id,
                self.config.dede_user_id_ckmd5
            )
        };

        let response: Value = client
            .get(&format!(
                "https://api.live.bilibili.com/xlive/web-room/v1/index/getDanmuInfo?{}",
                query_string
            ))
            .header("Cookie", &cookie)
            .send()
            .await?
            .json()
            .await?;

        let code = response["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let message = response["message"].as_str().unwrap_or("Unknown error");
            error!("Danmaku API error - Code: {}, Message: {}", code, message);
            error!(
                "Full response: {}",
                serde_json::to_string_pretty(&response).unwrap_or_default()
            );
            return Err(anyhow::anyhow!(
                "Failed to get danmaku info: Code {}, Message: {}",
                code,
                message
            ));
        }

        let data = &response["data"];
        self.token = data["token"].as_str().map(|s| s.to_string());

        if let Some(host_list) = data["host_list"].as_array() {
            self.host_list = host_list
                .iter()
                .filter_map(|host| host["host"].as_str().map(|s| s.to_string()))
                .collect();
        }

        if self.host_list.is_empty() {
            return Err(anyhow::anyhow!("No danmaku hosts available"));
        }

        // info!(
        //     "Successfully got danmaku info - Token: {}, Hosts: {:?}",
        //     self.token.as_deref().unwrap_or("none"),
        //     self.host_list
        // );

        Ok(())
    }

    fn create_auth_packet(&self) -> Result<Vec<u8>> {
        // Certificate packet for authentication
        // Like the reference implementation: uses token from getDanmuInfo
        let uid = self.config.dede_user_id.parse::<u64>().unwrap_or(0);
        let token = self.token.as_deref().unwrap_or("");

        let auth_data = serde_json::json!({
            "uid": uid,
            "roomid": self.room_id,
            "protover": 2,
            "platform": "web",
            "type": 2,
            "key": token
        });

        let body = serde_json::to_vec(&auth_data)?;
        self.create_packet(OP_AUTH, &body)
    }

    fn create_heartbeat_packet(&self) -> Vec<u8> {
        self.create_packet(OP_HEARTBEAT, &[]).unwrap()
    }

    fn create_packet(&self, operation: u32, body: &[u8]) -> Result<Vec<u8>> {
        let mut packet = Vec::new();

        // Packet length (header + body)
        packet.write_u32::<BigEndian>(HEADER_LENGTH + body.len() as u32)?;

        // Header length
        packet.write_u16::<BigEndian>(HEADER_LENGTH as u16)?;

        // Protocol - COMMAND (0) for regular packets, SPECIAL (1) for auth
        packet.write_u16::<BigEndian>(PROTOCOL_COMMAND)?;

        // Operation
        packet.write_u32::<BigEndian>(operation)?;

        // Sequence (always 1)
        packet.write_u32::<BigEndian>(1)?;

        // Body
        packet.extend_from_slice(body);

        Ok(packet)
    }

    async fn handle_message(&self, data: &[u8]) -> Result<()> {
        let mut cursor = Cursor::new(data);

        while cursor.position() < data.len() as u64 {
            // Read packet header
            let packet_length = cursor.read_u32::<BigEndian>()?;
            let header_length = cursor.read_u16::<BigEndian>()?;
            let protocol_version = cursor.read_u16::<BigEndian>()?;
            let operation = cursor.read_u32::<BigEndian>()?;
            let _sequence = cursor.read_u32::<BigEndian>()?;

            let body_length = packet_length - header_length as u32;
            let mut body = vec![0u8; body_length as usize];
            cursor.read_exact(&mut body)?;

            match operation {
                OP_AUTH_REPLY => {
                    // info!("Authentication successful");
                }
                OP_HEARTBEAT_REPLY => {
                    // Heartbeat reply contains viewer count
                    // if body.len() >= 4 {
                    //     let viewer_count = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    //     info!("Viewer count: {}", viewer_count);
                    // }
                }
                OP_MESSAGE => {
                    self.handle_danmaku_message(protocol_version, &body).await?;
                }
                _ => {
                    // Unknown operation
                }
            }
        }

        Ok(())
    }

    async fn handle_danmaku_message(&self, protocol_version: u16, body: &[u8]) -> Result<()> {
        let decompressed_data = match protocol_version {
            PROTOCOL_COMMAND_ZLIB => {
                let mut decoder = ZlibDecoder::new(body);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed)?;
                decompressed
            }
            PROTOCOL_COMMAND_BROTLI => {
                // For now, skip brotli decompression as it requires additional dependency
                // You can add brotli support later if needed
                return Ok(());
            }
            _ => body.to_vec(),
        };

        // Parse nested messages
        if protocol_version == PROTOCOL_COMMAND_ZLIB {
            Box::pin(self.handle_message(&decompressed_data)).await?;
        } else {
            // Parse JSON message
            if let Ok(json_str) = String::from_utf8(decompressed_data) {
                if let Ok(message) = serde_json::from_str::<DanmakuMessage>(&json_str) {
                    self.process_danmaku_command(&message).await;
                }
            }
        }

        Ok(())
    }

    async fn process_danmaku_command(&self, message: &DanmakuMessage) {
        match message.cmd.as_str() {
            "DANMU_MSG" => {
                if self.enable_commands.load(Ordering::Relaxed) {
                    if let Some(info) = &message.info {
                        if let Some(info_array) = info.as_array() {
                            if info_array.len() > 2 {
                                // Extract danmaku text and user info
                                let danmaku_text = info_array[1].as_str().unwrap_or("");
                                if danmaku_text.contains("%查询") || danmaku_text.contains("%转播%")
                                {
                                    let formatted_message = format!(" :{}", danmaku_text);
                                    crate::plugins::danmaku::process_danmaku(&formatted_message)
                                        .await;
                                    let user_info = info_array[2].as_array();
                                    let username = user_info
                                        .and_then(|u| u.get(1))
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("Unknown");

                                    info!("💬 [{}]: {}", username, danmaku_text);
                                }
                            }
                        }
                    }
                }
            }
            "LIVE" => {
                // if let Some(data) = &message.data {
                //     let room_id = data["room_id"].as_u64().unwrap_or(0);
                //     info!("🔴 Live started - Room ID: {}", room_id);
                // }
            }
            "PREPARING" => {
                // if let Some(data) = &message.data {
                //     let room_id = data["roomid"].as_str().unwrap_or("unknown");
                //     info!("⚫ Live stopped - Room ID: {}", room_id);
                // }
            }
            "WARNING" => {
                if let Some(data) = &message.data {
                    let msg = data["msg"].as_str().unwrap_or("No message");
                    warn!("⚠️ Warning: {}", msg);
                    let cfg = self.app_config.clone();
                    tokio::spawn(async move {
                        // Get current streaming channel from bili title
                        if let Ok((_, title, _)) =
                            crate::plugins::get_bili_live_status(cfg.bililive.room).await
                        {
                            if title.contains("【转播】") {
                                let channel_name = title.split("【转播】").last().unwrap_or("");
                                if !channel_name.is_empty() {
                                    // Set warning flag to prevent restreaming this channel
                                    crate::plugins::danmaku::set_warning_stop(
                                        channel_name.to_string(),
                                    );
                                    info!("🚫 已标记频道 {} 为警告状态，将跳过转播", channel_name);
                                }
                            }
                        }

                        if let Err(e) = bili_stop_live(&cfg).await {
                            error!("Failed to stop live on warning: {}", e);
                        }
                    });
                }
            }
            "CUT_OFF" => {
                if let Some(data) = &message.data {
                    let msg = data["msg"].as_str().unwrap_or("Stream cut off");
                    warn!("✂️ Cut off: {}", msg)
                };
                let cfg = self.app_config.clone();
                tokio::spawn(async move {
                    // Get current streaming channel from bili title
                    if let Ok((_, title, _)) =
                        crate::plugins::get_bili_live_status(cfg.bililive.room).await
                    {
                        if title.contains("【转播】") {
                            let channel_name = title.split("【转播】").last().unwrap_or("");
                            if !channel_name.is_empty() {
                                // Set warning flag to prevent restreaming this channel
                                crate::plugins::danmaku::set_warning_stop(channel_name.to_string());
                                info!("🚫 已标记频道 {} 为警告状态，将跳过转播", channel_name);
                            }
                        }
                    }

                    if let Err(e) = bili_stop_live(&cfg).await {
                        error!("Failed to stop live on warning: {}", e);
                    }
                });
                // if let Some(data) = &message.data {
                //     let username = data["uname"].as_str().unwrap_or("User");
                //     info!("👋 {} entered the room", username);
                // }
            }
            "WELCOME_GUARD" => {
                // if let Some(data) = &message.data {
                //     let username = data["username"].as_str().unwrap_or("Guard");
                //     info!("🛡️ Guard {} entered the room", username);
                // }
            }
            "SEND_GIFT" => {
                if let Some(data) = &message.data {
                    let username = data["uname"].as_str().unwrap_or("User");
                    let gift_name = data["giftName"].as_str().unwrap_or("gift");
                    let num = data["num"].as_u64().unwrap_or(1);
                    info!("🎁 {} sent {} x{}", username, gift_name, num);
                    let cfg = self.app_config.clone();
                    let thank_msg = format!("谢谢{}送的{}", username, gift_name);
                    tokio::spawn(async move {
                        if let Err(e) = send_danmaku(&cfg, &thank_msg).await {
                            error!("Failed to send thank you danmaku: {}", e);
                        }
                    });
                }
            }
            "SUPER_CHAT_MESSAGE" | "SUPER_CHAT_MESSAGE_JP" => {
                // if let Some(data) = &message.data {
                //     let username = data["user_info"]["uname"].as_str().unwrap_or("User");
                //     let message_text = data["message"].as_str().unwrap_or("");
                //     let price = data["price"].as_u64().unwrap_or(0);
                //     info!(
                //         "💰 {} sent Super Chat (¥{}): {}",
                //         username, price, message_text
                //     );
                // }
            }
            "GUARD_BUY" => {
                // if let Some(data) = &message.data {
                //     let username = data["username"].as_str().unwrap_or("User");
                //     let gift_name = data["gift_name"].as_str().unwrap_or("Guard");
                //     let num = data["num"].as_u64().unwrap_or(1);
                //     info!("🛡️ {} purchased {} x{}", username, gift_name, num);
                // }
            }
            "INTERACT_WORD" | "INTERACT_WORD_V2" => {
                // User interaction (enter room, follow, etc.) - suppress (too frequent)
            }
            "NOTICE_MSG" => {
                // Notice messages - suppress
            }
            "GIFT_TOP" => {
                // Gift ranking - suppress
            }
            "ROOM_REAL_TIME_MESSAGE_UPDATE" => {
                // Room stats update - suppress (too frequent)
            }
            "ONLINE_RANK_V2" | "ONLINE_RANK_COUNT" | "ONLINE_RANK_V3" => {
                // Online rank updates - suppress (not important)
            }
            "STOP_LIVE_ROOM_LIST" => {
                // Stop live room list - suppress
            }
            "WATCHED_CHANGE" => {
                // Watched count change - suppress
            }
            _ => {
                // Log unknown message types for debugging
                warn!("📨 Unknown message type: {}", message.cmd);
            }
        }
    }
}

pub async fn run_native_danmaku_client(
    config: DanmakuConfig,
    app_config: Arc<Config>,
    enable_commands: Arc<AtomicBool>,
) -> Result<()> {
    let mut client = BilibiliDanmakuClient::new(config, app_config, enable_commands);

    loop {
        match client.connect().await {
            Ok(_) => {
                info!("Danmaku client disconnected normally");
                break;
            }
            Err(e) => {
                error!("Danmaku client error: {}", e);
                info!("Reconnecting in 5 seconds...");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    Ok(())
}
