use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use flate2::read::ZlibDecoder;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{interval, timeout, Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Bytes, tungstenite::Message};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::plugins::{bili_stop_live, send_danmaku};

// Bilibili danmaku protocol constants
const HEADER_LENGTH: u32 = 16;
const MAX_DECODED_BYTES: usize = 16 * 1024 * 1024;
const MAX_NESTING_DEPTH: usize = 8;
const MAX_PACKETS: usize = 4096;

// Protocol types (header protocol field)
const PROTOCOL_COMMAND: u16 = 0;
const PROTOCOL_COMMAND_ZLIB: u16 = 2;
const PROTOCOL_COMMAND_BROTLI: u16 = 3;

// Operation codes (packet type)
const OP_HEARTBEAT: u32 = 2;
const OP_MESSAGE: u32 = 5;
const OP_AUTH: u32 = 7;

// Protocol versions (body protover field)
#[allow(dead_code)]
const PROTOVER_NORMAL: u8 = 1;
#[allow(dead_code)]
const PROTOVER_BROTLI: u8 = 3;

fn danmaku_packet_body_length(packet_length: u32, header_length: u16) -> Result<u32> {
    if header_length as u32 != HEADER_LENGTH {
        return Err(anyhow!(
            "unsupported danmaku packet header length: {}",
            header_length
        ));
    }
    if packet_length < HEADER_LENGTH {
        return Err(anyhow!(
            "invalid danmaku packet length: packet={} header={}",
            packet_length,
            header_length
        ));
    }
    Ok(packet_length - HEADER_LENGTH)
}

fn decode_danmaku_body<'a>(
    protocol_version: u16,
    body: &'a [u8],
    bytes_left: &mut usize,
) -> Result<(Cow<'a, [u8]>, bool)> {
    fn limited(reader: impl Read, bytes_left: &mut usize) -> Result<Vec<u8>> {
        let mut decoded = Vec::new();
        reader
            .take(*bytes_left as u64 + 1)
            .read_to_end(&mut decoded)?;
        *bytes_left = bytes_left
            .checked_sub(decoded.len())
            .ok_or_else(|| anyhow!("danmaku decoded byte budget exceeded"))?;
        Ok(decoded)
    }
    let decoded = match protocol_version {
        PROTOCOL_COMMAND_ZLIB => limited(ZlibDecoder::new(body), bytes_left)?,
        PROTOCOL_COMMAND_BROTLI => limited(brotli::Decompressor::new(body, 4096), bytes_left)?,
        _ => return Ok((Cow::Borrowed(body), false)),
    };
    Ok((Cow::Owned(decoded), true))
}

/// Validate a complete frame before dispatching commands. Limits apply to the
/// entire expansion tree, including sibling compressed packets.
fn decode_danmaku_messages(data: &[u8]) -> Result<Vec<DanmakuMessage>> {
    fn packets(
        data: &[u8],
        depth: usize,
        bytes_left: &mut usize,
        packets_left: &mut usize,
        messages: &mut Vec<DanmakuMessage>,
    ) -> Result<()> {
        if depth > MAX_NESTING_DEPTH {
            return Err(anyhow!("danmaku nesting limit exceeded"));
        }
        let mut cursor = Cursor::new(data);
        while cursor.position() < data.len() as u64 {
            *packets_left = packets_left
                .checked_sub(1)
                .ok_or_else(|| anyhow!("danmaku packet budget exceeded"))?;
            let packet_length = cursor.read_u32::<BigEndian>()?;
            let header_length = cursor.read_u16::<BigEndian>()?;
            let protocol_version = cursor.read_u16::<BigEndian>()?;
            let operation = cursor.read_u32::<BigEndian>()?;
            let _sequence = cursor.read_u32::<BigEndian>()?;
            let body_length = danmaku_packet_body_length(packet_length, header_length)? as usize;
            let start = cursor.position() as usize;
            let end = start
                .checked_add(body_length)
                .filter(|end| *end <= data.len())
                .ok_or_else(|| anyhow!("truncated danmaku packet body"))?;
            cursor.set_position(end as u64);
            if operation != OP_MESSAGE {
                continue;
            }
            let (body, nested) =
                decode_danmaku_body(protocol_version, &data[start..end], bytes_left)?;
            if nested {
                packets(&body, depth + 1, bytes_left, packets_left, messages)?;
            } else if let Ok(message) = serde_json::from_slice(&body) {
                messages.push(message);
            }
        }
        Ok(())
    }
    let mut bytes_left = MAX_DECODED_BYTES
        .checked_sub(data.len())
        .ok_or_else(|| anyhow!("danmaku frame byte budget exceeded"))?;
    let mut messages = Vec::new();
    let mut packets_left = MAX_PACKETS;
    packets(data, 0, &mut bytes_left, &mut packets_left, &mut messages)?;
    Ok(messages)
}

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
        ws_sender.send(Message::Binary(auth_packet.into())).await?;

        // Start heartbeat task
        let mut heartbeat_interval = interval(Duration::from_secs(30));
        // Held as Bytes so the per-tick clone below is a refcount bump, not a copy.
        let heartbeat_packet: Bytes = Self::create_heartbeat_packet()?.into();

        // Track last activity for connection health monitoring
        let mut last_activity = Instant::now();
        let connection_timeout = Duration::from_secs(120); // 2 minutes without any activity

        loop {
            // Check for stop signal
            if crate::plugins::danmaku::should_stop_danmaku() {
                info!("🛑 收到停止信号，断开弹幕连接");
                break;
            }

            tokio::select! {
                _ = crate::plugins::danmaku::wait_danmaku_stop_signal() => {
                    info!("🛑 收到停止信号，断开弹幕连接");
                    break;
                }
                // Handle incoming messages with timeout
                msg = timeout(Duration::from_secs(60), ws_receiver.next()) => {
                    match msg {
                        Ok(Some(Ok(Message::Binary(data)))) => {
                            last_activity = Instant::now(); // Update activity timestamp
                            if let Err(e) = self.handle_message(&data).await {
                                error!("Error handling message: {}", e);
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) => {
                            warn!("WebSocket connection closed by server");
                            break;
                        }
                        Ok(Some(Err(e))) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        Ok(None) => {
                            warn!("WebSocket stream ended");
                            break;
                        }
                        Ok(Some(Ok(Message::Ping(data)))) => {
                            last_activity = Instant::now();
                            if let Err(e) = ws_sender.send(Message::Pong(data)).await {
                                error!("Failed to send pong: {}", e);
                                break;
                            }
                        }
                        Ok(Some(Ok(Message::Pong(_)))) => {
                            last_activity = Instant::now();
                        }
                        Err(_) => {
                            // Timeout occurred - check if connection is still alive
                            if last_activity.elapsed() > connection_timeout {
                                warn!("Connection timeout - no activity for {:?}", connection_timeout);
                                break;
                            }
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

                    // Also check for connection timeout during heartbeat
                    if last_activity.elapsed() > connection_timeout {
                        warn!("Connection appears stale - forcing reconnection");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    async fn get_danmaku_info(&mut self) -> Result<()> {
        let client = crate::plugins::bilibili::bili_plain_http_client();
        let mut params = BTreeMap::new();
        params.insert("id", self.room_id.to_string());
        params.insert("type", "0".to_string());
        let query_string = crate::plugins::wbi::signed_query(&client, params)
            .await
            .map_err(|e| anyhow!("{e}"))?;

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
            .get(format!(
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
        Self::create_packet(OP_AUTH, &body)
    }

    fn create_heartbeat_packet() -> Result<Vec<u8>> {
        Self::create_packet(OP_HEARTBEAT, &[])
    }

    fn create_packet(operation: u32, body: &[u8]) -> Result<Vec<u8>> {
        let body_len =
            u32::try_from(body.len()).map_err(|_| anyhow!("danmaku packet body too large"))?;
        let packet_len = HEADER_LENGTH
            .checked_add(body_len)
            .ok_or_else(|| anyhow!("danmaku packet length overflow"))?;
        let mut packet = Vec::with_capacity(packet_len as usize);

        // Packet length (header + body)
        packet.write_u32::<BigEndian>(packet_len)?;

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
        for message in decode_danmaku_messages(data)? {
            self.process_danmaku_command(&message).await;
        }
        Ok(())
    }

    async fn process_danmaku_command(&self, message: &DanmakuMessage) {
        match message.cmd.as_str() {
            "DANMU_MSG" => {
                if let Some(info) = &message.info {
                    if let Some(info_array) = info.as_array() {
                        if info_array.len() > 2 {
                            // Extract danmaku text and user info
                            let danmaku_text = info_array[1].as_str().unwrap_or("");
                            // Only allocate new string if replacement is needed
                            let danmaku_text = if danmaku_text.contains("％") {
                                danmaku_text.replace("％", "%")
                            } else {
                                danmaku_text.to_string()
                            };
                            let user_info = info_array[2].as_array();
                            let username = user_info
                                .and_then(|u| u.get(1))
                                .and_then(|n| n.as_str())
                                .unwrap_or("Unknown");
                            let uid = user_info
                                .and_then(|u| u.first())
                                .and_then(|n| n.as_u64())
                                .unwrap_or(0);

                            // Get owner UID from config
                            let owner_uid = self.config.dede_user_id.parse::<u64>().unwrap_or(0);

                            // Log every danmaku message
                            // info!("💬 [{}]: {}", username, danmaku_text);

                            // Process owner-only commands
                            if danmaku_text.contains("%查询") || danmaku_text.contains("%转播")
                            {
                                if uid == owner_uid {
                                    // Owner can always use all commands
                                    let formatted_message = format!(" :{}", danmaku_text);
                                    crate::plugins::danmaku::process_danmaku_with_owner(
                                        &formatted_message,
                                        true, // is_owner = true
                                    )
                                    .await;
                                    info!("🔧 {}", danmaku_text);
                                } else if self.enable_commands.load(Ordering::Relaxed)
                                    && danmaku_text.contains("%转播")
                                {
                                    // Non-owners can use commands "%转播" only when enabled
                                    let formatted_message = format!(" :{}", danmaku_text);
                                    info!("💬 {} : {}", username, danmaku_text);
                                    crate::plugins::danmaku::process_danmaku_with_owner(
                                        &formatted_message,
                                        false, // is_owner = false
                                    )
                                    .await;
                                } else if danmaku_text.contains("%查询") {
                                    let formatted_message = format!(" :{}", danmaku_text);
                                    crate::plugins::danmaku::process_danmaku_with_owner(
                                        &formatted_message,
                                        false, // is_owner = false
                                    )
                                    .await;
                                } else {
                                    info!("🚫 Command ignored");
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
                        match crate::plugins::get_bili_live_status(cfg.bililive.room).await {
                            Ok((_, title, _)) => {
                                if title.contains("【转播】") {
                                    let channel_name = title.split("【转播】").last().unwrap_or("");
                                    if !channel_name.is_empty() {
                                        // Set warning flag to prevent restreaming this channel
                                        crate::plugins::danmaku::set_warning_stop(
                                            channel_name.to_string(),
                                        );
                                        info!(
                                            "🚫 已标记频道 {} 为警告状态，将跳过转播",
                                            channel_name
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to get bili live status: {}", e.to_string());
                            }
                        }

                        if let Err(e) = bili_stop_live(&cfg).await {
                            error!("Failed to stop live on warning: {}", e);
                        }
                        if let Err(e) = send_danmaku(&cfg, "🚫 警告/切断状态，请换台").await
                        {
                            error!("Failed to send warning danmaku: {}", e.to_string());
                        }
                    });
                }
            }
            "CUT_OFF" | "CUT_OFF_V2" | "ANCHOR_ECOLOGY_LIVING_DIALOG" | "FULL_SCREEN_MASK_OPEN" => {
                if let Some(data) = &message.data {
                    let msg = if message.cmd == "CUT_OFF" || message.cmd == "CUT_OFF_V2" {
                        data["msg"].as_str().unwrap_or("Stream cut off")
                    } else if message.cmd == "ANCHOR_ECOLOGY_LIVING_DIALOG" {
                        data["dialog_title"].as_str().unwrap_or("直播间违规")
                    } else {
                        data["title"].as_str().unwrap_or("直播间涉嫌违规")
                    };
                    warn!("✂️ Cut off/Warning: {}", msg)
                };
                let cfg = self.app_config.clone();
                tokio::spawn(async move {
                    // Get current streaming channel from bili title
                    match crate::plugins::get_bili_live_status(cfg.bililive.room).await {
                        Ok((_, title, _)) => {
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
                        Err(e) => {
                            error!("Failed to get bili live status: {}", e);
                        }
                    }
                    if let Err(e) = send_danmaku(&cfg, "🚫 警告/切断状态，请换台").await
                    {
                        error!("Failed to send warning danmaku: {}", e);
                    }

                    if let Err(e) = bili_stop_live(&cfg).await {
                        error!("Failed to stop live on warning: {}", e);
                    }
                });
            }
            "WELCOME_GUARD" => {
                // if let Some(data) = &message.data {
                //     let username = data["username"].as_str().unwrap_or("Guard");
                //     info!("🛡️ Guard {} entered the room", username);
                // }
            }
            "SEND_GIFT" => {
                // if let Some(data) = &message.data {
                //     let username = data["uname"].as_str().unwrap_or("User");
                //     let gift_name = data["giftName"].as_str().unwrap_or("gift");
                //     let num = data["num"].as_u64().unwrap_or(1);
                //     info!("🎁 {} sent {} x{}", username, gift_name, num);
                //     let cfg = self.app_config.clone();
                //     let thank_msg = format!("谢谢{}送的{}", username, gift_name);
                //     tokio::spawn(async move {
                //         if let Err(e) = send_danmaku(&cfg, &thank_msg).await {
                //             error!("Failed to send thank you danmaku: {}", e);
                //         }
                //     });
                // }
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
            "COVER_AUDIT_STATUS_CHANGED"
            | "VOICE_REPORT_LIKE"
            | "FLOW_REWARD_CARD"
            | "COMMON_NOTICE_DANMAKU"
            | "LIVE_ANI_RES_UPDATE"
            | "PK_BATTLE_ENTRANCE"
            | "MESSAGEBOX_USER_MEDAL_CHANGE"
            | "SPREAD_SHOW_FEET"
            | "SPREAD_SHOW_FEET_V2"
            | "TRADING_SCORE"
            | "UNIVERSAL_INTERACT_INVITATION"
            | "LIKE_GUIDE_USER"
            | "PLAYURL_RELOAD"
            | "WIDGET_GIFT_STAR_PROCESS"
            | "COMBO_END"
            | "POPULAR_RANK_CHANGED"
            | "master_qn_strategy_chg"
            | "GUARD_HONOR_THOUSAND"
            | "DM_INTERACTION"
            | "ONLINE_RANK_V2"
            | "ONLINE_RANK_COUNT"
            | "ONLINE_RANK_V3"
            | "RANK_REM"
            | "STOP_LIVE_ROOM_LIST"
            | "WATCHED_CHANGE"
            | "LIKE_INFO_V3_NOTICE"
            | "LIKE_INFO_V3_UPDATE"
            | "LIKE_INFO_V3_CLICK"
            | "WIDGET_BANNER"
            | "POPULARITY_RANK_TAB_CHG"
            | "ROOM_LIVE_FORBID"
            | "ROOM_CHANGE"
            | "ANCHOR_LOT_NOTICE"
            | "ANCHOR_HELPER_DANMU"
            | "PLAYTOGETHER_ICON_CHANGE"
            | "CHG_RANK_REFRESH"
            | "VOICE_JOIN_ROOM_COUNT_INFO"
            | "VOICE_JOIN_LIST"
            | "RANK_CHANGED"
            | "RANK_CHANGED_V2"
            | "ENTRY_EFFECT" => {}
            "ROOM_CONTENT_AUDIT_REPORT" => {
                if let Some(data) = &message.data {
                    if let Some(title) = data["audit_title"].as_str() {
                        info!("标题修改成功：{}", title);
                    }
                } else {
                    println!("Unknown message: {:?}", message);
                }
            }
            _ => {
                // Log unknown message types for debugging

                warn!("📨 Unknown message type: {}", message.cmd);

                println!("message content : {:?}", message);
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
    let mut reconnect_attempts = 0;
    let max_reconnect_attempts = 10;

    loop {
        // Check for stop signal
        if crate::plugins::danmaku::should_stop_danmaku() {
            info!("🛑 收到停止信号，退出弹幕客户端");
            break;
        }

        match client.connect().await {
            Ok(_) => {
                info!("Danmaku client disconnected normally");
                reconnect_attempts = 0; // Reset counter on successful connection

                // Check if this was an intentional stop
                if crate::plugins::danmaku::should_stop_danmaku() {
                    info!("🛑 停止信号已确认，退出弹幕客户端");
                    break;
                }

                // Otherwise it was unexpected - try to reconnect
                warn!("Unexpected disconnection, attempting to reconnect...");
                tokio::select! {
                    _ = crate::plugins::danmaku::wait_danmaku_stop_signal() => {
                        info!("🛑 收到停止信号，退出弹幕客户端");
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
            Err(e) => {
                reconnect_attempts += 1;
                error!(
                    "Danmaku client error (attempt {}): {}",
                    reconnect_attempts, e
                );

                if reconnect_attempts >= max_reconnect_attempts {
                    error!("Max reconnection attempts reached, giving up");
                    break;
                }

                // Exponential backoff with jitter
                let delay = std::cmp::min(5 * reconnect_attempts, 60);
                info!("Reconnecting in {} seconds...", delay);
                tokio::select! {
                    _ = crate::plugins::danmaku::wait_danmaku_stop_signal() => {
                        info!("🛑 收到停止信号，退出弹幕客户端");
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(delay as u64)) => {}
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn packet(protocol: u16, body: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet
            .write_u32::<BigEndian>(HEADER_LENGTH + body.len() as u32)
            .unwrap();
        packet.write_u16::<BigEndian>(HEADER_LENGTH as u16).unwrap();
        packet.write_u16::<BigEndian>(protocol).unwrap();
        packet.write_u32::<BigEndian>(OP_MESSAGE).unwrap();
        packet.write_u32::<BigEndian>(1).unwrap();
        packet.extend_from_slice(body);
        packet
    }

    fn compressed_packet(body: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(body).unwrap();
        packet(PROTOCOL_COMMAND_ZLIB, &encoder.finish().unwrap())
    }

    #[test]
    fn decoded_budget_is_shared_by_sibling_packets() {
        let compressed = compressed_packet(&vec![0; MAX_DECODED_BYTES / 2 + 1]);
        let body = &compressed[HEADER_LENGTH as usize..];
        let mut budget = MAX_DECODED_BYTES;
        decode_danmaku_body(PROTOCOL_COMMAND_ZLIB, body, &mut budget).unwrap();
        assert!(decode_danmaku_body(PROTOCOL_COMMAND_ZLIB, body, &mut budget).is_err());
    }

    #[test]
    fn nested_packets_preserve_order_and_reject_excessive_depth() {
        let mut nested = packet(PROTOCOL_COMMAND, br#"{"cmd":"first"}"#);
        nested.extend(packet(PROTOCOL_COMMAND, br#"{"cmd":"second"}"#));
        for _ in 0..MAX_NESTING_DEPTH {
            nested = compressed_packet(&nested);
        }
        let messages = decode_danmaku_messages(&nested).unwrap();
        assert_eq!(
            messages.iter().map(|m| m.cmd.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(decode_danmaku_messages(&compressed_packet(&nested)).is_err());
    }

    #[test]
    fn rejects_packet_floods_and_truncated_bodies() {
        let one = packet(PROTOCOL_COMMAND, b"{}");
        assert!(decode_danmaku_messages(&one.repeat(MAX_PACKETS + 1)).is_err());
        assert!(decode_danmaku_messages(&one[..one.len() - 1]).is_err());
    }

    #[test]
    fn heartbeat_packet_has_valid_header() {
        let packet = BilibiliDanmakuClient::create_heartbeat_packet()
            .expect("heartbeat packet should be created");
        let mut cursor = Cursor::new(packet.as_slice());

        assert_eq!(cursor.read_u32::<BigEndian>().unwrap(), HEADER_LENGTH);
        assert_eq!(
            cursor.read_u16::<BigEndian>().unwrap(),
            HEADER_LENGTH as u16
        );
        assert_eq!(cursor.read_u16::<BigEndian>().unwrap(), PROTOCOL_COMMAND);
        assert_eq!(cursor.read_u32::<BigEndian>().unwrap(), OP_HEARTBEAT);
    }

    #[test]
    fn packet_body_length_rejects_malformed_header() {
        assert!(danmaku_packet_body_length(HEADER_LENGTH, 12).is_err());
        assert!(danmaku_packet_body_length(HEADER_LENGTH - 1, HEADER_LENGTH as u16).is_err());
        assert_eq!(
            danmaku_packet_body_length(HEADER_LENGTH + 8, HEADER_LENGTH as u16).unwrap(),
            8
        );
    }

    #[test]
    fn brotli_danmaku_body_is_decoded() {
        let payload = br#"{"cmd":"DANMU_MSG","info":[[],"hello",[1,"user"]]}"#;
        let mut compressed = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            writer.write_all(payload).unwrap();
        }

        let mut budget = MAX_DECODED_BYTES;
        let (decoded, nested) =
            decode_danmaku_body(PROTOCOL_COMMAND_BROTLI, &compressed, &mut budget).unwrap();

        assert!(nested);
        assert_eq!(decoded.as_ref(), payload);
    }
}
