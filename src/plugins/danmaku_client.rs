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
use std::sync::{Arc, Mutex};
use tokio::time::{interval, timeout, Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Bytes, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::logging::{safe_diagnostic, safe_text};
use crate::plugins::danmaku::warning::{
    log_warning_mark, warning_tracker, WarningEvent, WarningTracker,
};
use crate::plugins::{bili_stop_live, send_danmaku};

pub(crate) fn capture_warning_message(
    warnings: &WarningTracker,
    message: &DanmakuMessage,
) -> Option<WarningEvent> {
    let cmd = message.cmd.as_str();
    if !matches!(
        cmd,
        "WARNING"
            | "CUT_OFF"
            | "CUT_OFF_V2"
            | "ANCHOR_ECOLOGY_LIVING_DIALOG"
            | "FULL_SCREEN_MASK_OPEN"
            | "ROOM_LOCK"
    ) {
        return None;
    }
    if cmd == "WARNING" {
        let text = message
            .data
            .as_ref()
            .and_then(|data| data["msg"].as_str())
            .unwrap_or("No message");
        warn!("Warning: {}", safe_text(text, 160));
    } else if cmd != "ROOM_LOCK" {
        let key = if cmd == "ANCHOR_ECOLOGY_LIVING_DIALOG" {
            "dialog_title"
        } else if cmd == "FULL_SCREEN_MASK_OPEN" {
            "title"
        } else {
            "msg"
        };
        let text = message
            .data
            .as_ref()
            .and_then(|data| data[key].as_str())
            .unwrap_or("Stream cut off");
        warn!("Cut off/Warning: {}", safe_text(text, 160));
    }
    let mut event = warnings.capture_event();
    if cmd != "ROOM_LOCK" {
        if let Some(name) = warnings.mark_event(&mut event, None) {
            log_warning_mark(&name);
        }
    }
    Some(event)
}

/// Complete title-only fallback while retaining the source captured at receipt.
/// No delayed response may attach itself to a replacement ffmpeg session.
async fn complete_warning_event(
    mut event: WarningEvent,
    room: i32,
    timed_lock: bool,
) -> WarningEvent {
    let warnings = warning_tracker();
    if timed_lock || !event.has_source() {
        let title = if event.has_source() {
            None
        } else {
            match crate::plugins::get_bili_live_status(room).await {
                Ok(status) => Some(status.title),
                Err(error) => {
                    error!(
                        "获取警告时的B站标题失败；{}",
                        safe_diagnostic(&error.to_string())
                    );
                    None
                }
            }
        };
        let marked = if timed_lock {
            warnings.mark_timed_event(&mut event, title.as_deref())
        } else {
            warnings.mark_event(&mut event, title.as_deref())
        };
        if let Some(name) = marked {
            log_warning_mark(&name);
        }
    }
    event
}

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

/// `Debug` is manual: cookie values show only whether they are set.
#[derive(Clone, Serialize, Deserialize)]
pub struct DanmakuConfig {
    pub room_id: u64,
    pub sessdata: String,
    pub bili_jct: String,
    pub dede_user_id: String,
    pub dede_user_id_ckmd5: String,
    pub buvid3: String,
}

impl std::fmt::Debug for DanmakuConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::config::secret;
        f.debug_struct("DanmakuConfig")
            .field("room_id", &self.room_id)
            .field("sessdata", &secret(&self.sessdata))
            .field("bili_jct", &secret(&self.bili_jct))
            .field("dede_user_id", &secret(&self.dede_user_id))
            .field("dede_user_id_ckmd5", &secret(&self.dede_user_id_ckmd5))
            .field("buvid3", &secret(&self.buvid3))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DanmakuMessage {
    pub cmd: String,
    pub info: Option<Value>,
    pub data: Option<Value>,
}

/// One room's last observed values. Identity is captured before any queued
/// work, not inferred from whichever relay session the writer later sees.
#[derive(Default)]
struct AudienceCounts {
    identity: Option<(u64, u64)>,
    online: Option<u64>,
    watched: Option<u64>,
    emitted: (Option<u64>, Option<u64>),
    last_emit: Option<Instant>,
}

fn room_history_event(
    message: &DanmakuMessage,
    room: u64,
    seq: Option<u64>,
) -> Option<crate::history::EventKind> {
    use crate::history::EventKind;
    let data = message.data.as_ref();
    let (key, fallback) = match message.cmd.as_str() {
        "WARNING" => ("msg", "直播间警告"),
        "CUT_OFF" | "CUT_OFF_V2" => ("msg", "直播间切断"),
        "ANCHOR_ECOLOGY_LIVING_DIALOG" => ("dialog_title", "直播间违规"),
        "FULL_SCREEN_MASK_OPEN" => ("title", "直播间涉嫌违规"),
        _ => return None,
    };
    let msg = safe_text(
        data.and_then(|data| data.get(key))
            .and_then(Value::as_str)
            .unwrap_or(fallback),
        160,
    );
    Some(if message.cmd == "WARNING" {
        EventKind::BiliWarning { seq, room, msg }
    } else {
        EventKind::BiliCutOff { seq, room, msg }
    })
}

impl AudienceCounts {
    fn observe(
        &mut self,
        room: u64,
        seq: Option<u64>,
        message: &DanmakuMessage,
        now: Instant,
        sample_secs: u64,
    ) -> Option<crate::history::EventKind> {
        let identity = seq.map(|seq| (room, seq));
        if self.identity != identity {
            self.identity = identity;
            self.online = None;
            self.watched = None;
            self.emitted = (None, None);
            // Preserve the room's throttle across a relay-session boundary.
            // Values from the preceding session must never follow it.
        }
        let (_, seq) = identity?;
        let data = message.data.as_ref()?;
        if let Some(packet_room) = data.get("room_id").or_else(|| data.get("roomid")) {
            let packet_room = packet_room
                .as_u64()
                .or_else(|| packet_room.as_str()?.parse().ok());
            if packet_room != Some(room) {
                return None;
            }
        }
        match message.cmd.as_str() {
            "WATCHED_CHANGE" => self.watched = Some(data.get("num")?.as_u64()?),
            "ONLINE_RANK_COUNT" => self.online = Some(data.get("online_count")?.as_u64()?),
            _ => return None,
        }
        let values = (self.online, self.watched);
        if let Some(last) = self.last_emit {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < Duration::from_secs(sample_secs.clamp(5, 60))
                || (values == self.emitted && elapsed < Duration::from_secs(60))
            {
                return None;
            }
        }
        self.last_emit = Some(now);
        self.emitted = values;
        Some(crate::history::EventKind::BiliCounts {
            seq,
            room,
            online: self.online,
            watched: self.watched,
        })
    }
}

pub struct BilibiliDanmakuClient {
    config: DanmakuConfig,
    room_id: u64,
    token: Option<String>,
    host_list: Vec<String>,
    app_config: Arc<Config>,
    enable_commands: Arc<AtomicBool>,
    audience: Mutex<AudienceCounts>,
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
            audience: Mutex::new(AudienceCounts::default()),
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
                info!("收到停止信号，断开弹幕连接");
                break;
            }

            tokio::select! {
                _ = crate::plugins::danmaku::wait_danmaku_stop_signal() => {
                    info!("收到停止信号，断开弹幕连接");
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
        // A preceding command in this frame can await remote work. Capture
        // every sample first so it cannot inherit a later relay's identity.
        let at = crate::storage::now();
        let observed_at = Instant::now();
        let seq = crate::history::recording_enabled()
            .then(|| crate::history::open_session_seq(self.room_id))
            .flatten()
            .filter(|_| crate::cluster::local_monitoring_allowed(&self.app_config));
        let messages = decode_danmaku_messages(data)?;
        self.record_history_messages(
            &messages,
            seq,
            at,
            observed_at,
            crate::history::bili_sample_secs(),
        );
        let warnings = warning_tracker();
        let warning_events: Vec<_> = messages
            .iter()
            .map(|message| capture_warning_message(&warnings, message))
            .collect();
        for (message, warning) in messages.into_iter().zip(warning_events) {
            self.process_captured_danmaku_command(&message, warning)
                .await;
        }
        Ok(())
    }

    fn record_history_messages(
        &self,
        messages: &[DanmakuMessage],
        seq: Option<u64>,
        at: u64,
        observed_at: Instant,
        sample_secs: u64,
    ) {
        for message in messages {
            match message.cmd.as_str() {
                "LIVE" => crate::history::bili::observe_room_status_at(self.room_id, true, at),
                "PREPARING" => {
                    crate::history::bili::observe_room_status_at(self.room_id, false, at)
                }
                _ => {}
            }
            let event = if matches!(message.cmd.as_str(), "WATCHED_CHANGE" | "ONLINE_RANK_COUNT") {
                self.audience
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .observe(self.room_id, seq, message, observed_at, sample_secs)
            } else {
                room_history_event(message, self.room_id, seq)
            };
            if let Some(event) = event {
                crate::history::emit_at(at, event);
            }
        }
    }

    #[cfg(test)]
    async fn process_danmaku_command(&self, message: &DanmakuMessage) {
        self.process_captured_danmaku_command(
            message,
            capture_warning_message(&warning_tracker(), message),
        )
        .await;
    }

    async fn process_captured_danmaku_command(
        &self,
        message: &DanmakuMessage,
        warning: Option<WarningEvent>,
    ) {
        match message.cmd.as_str() {
            // Captured as one observation above, before any command awaits.
            "WATCHED_CHANGE" | "ONLINE_RANK_COUNT" => {}
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
                            let uid = user_info
                                .and_then(|u| u.first())
                                .and_then(|n| n.as_u64())
                                .unwrap_or(0);

                            // Get owner UID from config
                            let owner_uid = self.config.dede_user_id.parse::<u64>().unwrap_or(0);

                            // Log every danmaku message
                            // info!("[{}]: {}", username, danmaku_text);

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
                                } else if self.enable_commands.load(Ordering::Relaxed)
                                    && danmaku_text.contains("%转播")
                                {
                                    // Non-owners can use commands "%转播" only when enabled
                                    let formatted_message = format!(" :{}", danmaku_text);
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
                                    crate::plugins::danmaku::reject_disabled_danmaku_target(
                                        &self.app_config.cluster.node_id,
                                        &danmaku_text,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            "LIVE" | "PREPARING" => {}
            "WARNING" => {
                let cfg = self.app_config.clone();
                if let Some(warning) = warning {
                    tokio::spawn(async move {
                        let warning =
                            complete_warning_event(warning, cfg.bililive.room, false).await;
                        if !warning_tracker().event_is_current(&warning) {
                            return;
                        }
                        match bili_stop_live(&cfg).await {
                            // The room is offline: this is the session's 下播
                            // cause, and the runtime cannot tell a cut from a
                            // plain stop (Addendum E rev 5 item 20).
                            Ok(()) => crate::notify::live_ended(
                                crate::events::BroadcastEndCause::WarningCut,
                            ),
                            Err(error) => {
                                error!(
                                    "Failed to stop live on warning: {}",
                                    safe_diagnostic(&error.to_string())
                                );
                            }
                        }
                        if !warning_tracker().event_is_current(&warning) {
                            return;
                        }
                        if let Err(error) = send_danmaku(&cfg, "警告/切断状态，请换台").await
                        {
                            error!(
                                "Failed to send warning danmaku: {}",
                                safe_diagnostic(&error.to_string())
                            );
                        }
                        warning_tracker().finish_unmarked_event(&warning);
                    });
                }
            }
            "CUT_OFF" | "CUT_OFF_V2" | "ANCHOR_ECOLOGY_LIVING_DIALOG" | "FULL_SCREEN_MASK_OPEN" => {
                let cfg = self.app_config.clone();
                if let Some(warning) = warning {
                    tokio::spawn(async move {
                        let warning =
                            complete_warning_event(warning, cfg.bililive.room, false).await;
                        if !warning_tracker().event_is_current(&warning) {
                            return;
                        }
                        if let Err(error) = send_danmaku(&cfg, "警告/切断状态，请换台").await
                        {
                            error!(
                                "Failed to send warning danmaku: {}",
                                safe_diagnostic(&error.to_string())
                            );
                        }
                        if !warning_tracker().event_is_current(&warning) {
                            return;
                        }
                        match bili_stop_live(&cfg).await {
                            Ok(()) => crate::notify::live_ended(
                                crate::events::BroadcastEndCause::WarningCut,
                            ),
                            Err(error) => {
                                error!(
                                    "Failed to stop live on warning: {}",
                                    safe_diagnostic(&error.to_string())
                                );
                            }
                        }
                        warning_tracker().finish_unmarked_event(&warning);
                    });
                }
            }
            "WELCOME_GUARD" => {
                // if let Some(data) = &message.data {
                //     let username = data["username"].as_str().unwrap_or("Guard");
                //     info!("Guard {} entered the room", username);
                // }
            }
            "SEND_GIFT" => {
                // if let Some(data) = &message.data {
                //     let username = data["uname"].as_str().unwrap_or("User");
                //     let gift_name = data["giftName"].as_str().unwrap_or("gift");
                //     let num = data["num"].as_u64().unwrap_or(1);
                //     info!("{} sent {} x{}", username, gift_name, num);
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
                //         "{} sent Super Chat (¥{}): {}",
                //         username, price, message_text
                //     );
                // }
            }
            "GUARD_BUY" => {
                // if let Some(data) = &message.data {
                //     let username = data["username"].as_str().unwrap_or("User");
                //     let gift_name = data["gift_name"].as_str().unwrap_or("Guard");
                //     let num = data["num"].as_u64().unwrap_or(1);
                //     info!("{} purchased {} x{}", username, gift_name, num);
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
            | "PLAYURL_RELOAD_MASTER"
            | "VOICE_JOIN_SWITCH"
            | "VOICE_JOIN_SWITCH_V2"
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
            | "ONLINE_RANK_V3"
            | "RANK_REM"
            | "STOP_LIVE_ROOM_LIST"
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
            "ROOM_LOCK" => {
                // The packet carries no duration. Preserve the room_init wait;
                // only a timed lock also holds this captured source channel.
                let room_id = self.app_config.bililive.room;
                warn!("B站房间锁定 ROOM_LOCK");
                tokio::spawn(async move {
                    let mut warning = warning;
                    match crate::plugins::get_bili_room_lock(room_id)
                        .await
                        .map_err(|error| error.to_string())
                    {
                        Ok(lock) if lock.is_locked => {
                            crate::plugins::announce_room_lock(room_id, &lock);
                            if crate::plugins::timed_room_lock_wait(&lock).is_some() {
                                if let Some(event) = warning.take() {
                                    warning =
                                        Some(complete_warning_event(event, room_id, true).await);
                                }
                            }
                        }
                        Ok(_) => warn!("收到 ROOM_LOCK，但 room_init 显示房间未锁定"),
                        Err(error) => error!(
                            "查询房间锁定状态失败: {}",
                            safe_diagnostic(&error.to_string())
                        ),
                    }
                    if let Some(warning) = warning {
                        warning_tracker().finish_unmarked_event(&warning);
                    }
                });
            }
            "ROOM_CONTENT_AUDIT_REPORT" => {
                if let Some(data) = &message.data {
                    if let Some(title) = data["audit_title"].as_str() {
                        info!("标题修改成功：{}", safe_text(title, 160));
                    }
                } else {
                    debug!("忽略缺少 data 的弹幕事件 type=ROOM_CONTENT_AUDIT_REPORT");
                }
            }
            _ => {
                debug!("忽略未处理的弹幕事件 type={}", safe_text(&message.cmd, 64));
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
            info!("收到停止信号，退出弹幕客户端");
            break;
        }

        let Some(result) = crate::plugins::danmaku::until_danmaku_stopped(client.connect()).await
        else {
            info!("收到停止信号，退出弹幕客户端");
            break;
        };
        match result {
            Ok(_) => {
                info!("Danmaku client disconnected normally");
                reconnect_attempts = 0; // Reset counter on successful connection

                // Check if this was an intentional stop
                if crate::plugins::danmaku::should_stop_danmaku() {
                    info!("停止信号已确认，退出弹幕客户端");
                    break;
                }

                // Otherwise it was unexpected - try to reconnect
                warn!("Unexpected disconnection, attempting to reconnect...");
                tokio::select! {
                    _ = crate::plugins::danmaku::wait_danmaku_stop_signal() => {
                        info!("收到停止信号，退出弹幕客户端");
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
                        info!("收到停止信号，退出弹幕客户端");
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
    use tracing::instrument::WithSubscriber;

    #[test]
    fn danmaku_config_debug_never_contains_cookie_values() {
        let config = DanmakuConfig {
            room_id: 1,
            sessdata: "synthetic-sessdata-d4".into(),
            bili_jct: "synthetic-csrf-d5".into(),
            dede_user_id: "synthetic-uid-d6".into(),
            dede_user_id_ckmd5: String::new(),
            buvid3: "synthetic-buvid-d7".into(),
        };
        let text = format!("{config:#?}");
        for value in [
            "synthetic-sessdata-d4",
            "synthetic-csrf-d5",
            "synthetic-uid-d6",
            "synthetic-buvid-d7",
        ] {
            assert!(!text.contains(value), "{value} leaked into Debug output");
        }
        assert!(text.contains("[redacted]") && text.contains("[empty]"));
    }

    fn client_for_messages() -> BilibiliDanmakuClient {
        BilibiliDanmakuClient::new(
            DanmakuConfig {
                room_id: 1,
                sessdata: String::new(),
                bili_jct: String::new(),
                dede_user_id: "1".into(),
                dede_user_id_ckmd5: String::new(),
                buvid3: String::new(),
            },
            Arc::new(crate::test_support::config("ca", 0)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn audience_message(cmd: &str, data: Value) -> DanmakuMessage {
        DanmakuMessage {
            cmd: cmd.into(),
            data: Some(data),
            info: None,
        }
    }

    #[test]
    fn audience_preserves_zero_unknown_fields_and_latest_values_with_room_throttle() {
        use crate::history::EventKind;
        let now = Instant::now();
        let mut counts = AudienceCounts::default();
        let zero = audience_message("ONLINE_RANK_COUNT", serde_json::json!({"online_count": 0}));
        assert!(matches!(
            counts.observe(1, Some(1), &zero, now, 1),
            Some(EventKind::BiliCounts {
                online: Some(0),
                watched: None,
                ..
            })
        ));
        let watched = audience_message("WATCHED_CHANGE", serde_json::json!({"num": 31}));
        assert!(counts
            .observe(1, Some(1), &watched, now + Duration::from_secs(1), 1)
            .is_none());
        assert!(counts
            .observe(1, Some(1), &zero, now + Duration::from_secs(4), 1)
            .is_none());
        assert!(matches!(
            counts.observe(1, Some(1), &zero, now + Duration::from_secs(5), 1),
            Some(EventKind::BiliCounts {
                online: Some(0),
                watched: Some(31),
                ..
            })
        ));
        for data in [
            serde_json::json!({}),
            serde_json::json!({"num": -1}),
            serde_json::json!({"num": "40"}),
        ] {
            assert!(counts
                .observe(
                    1,
                    Some(1),
                    &audience_message("WATCHED_CHANGE", data),
                    now + Duration::from_secs(10),
                    5
                )
                .is_none());
        }
        assert!(counts
            .observe(1, Some(1), &zero, now + Duration::from_secs(64), 5)
            .is_none());
        assert!(counts
            .observe(1, Some(1), &zero, now + Duration::from_secs(65), 5)
            .is_some());
        let other_room = audience_message(
            "WATCHED_CHANGE",
            serde_json::json!({"room_id": 2, "num": 999}),
        );
        assert!(counts
            .observe(1, Some(1), &other_room, now + Duration::from_secs(70), 5)
            .is_none());
        // No active relay: don't collect a standby's duplicate room samples.
        assert!(counts
            .observe(1, None, &watched, now + Duration::from_secs(71), 5)
            .is_none());
        assert!(matches!(
            counts.observe(1, Some(2), &watched, now + Duration::from_secs(72), 5),
            Some(EventKind::BiliCounts {
                seq: 2,
                online: None,
                watched: Some(31),
                ..
            })
        ));
    }

    #[test]
    fn decoded_counts_keep_observation_identity_when_writer_has_already_started_next_session() {
        use crate::history::event::Event;
        use crate::history::{EndReason, EventKind};
        let client = client_for_messages();
        let frame = packet(
            PROTOCOL_COMMAND,
            br#"{"cmd":"WATCHED_CHANGE","data":{"num":77,"room_id":1}}"#,
        );
        let messages = decode_danmaku_messages(&frame).unwrap();
        let bus = Arc::new(crate::events::Bus::new());
        let mut rx = bus.register("captured", 8, |_| true);
        bus.seal();
        crate::events::with_sync_test_bus(bus, || {
            client.record_history_messages(&messages, Some(7), 101, Instant::now(), 5);
        });
        let observation = rx.try_recv().unwrap();
        assert_eq!(observation.at, 101);
        assert!(matches!(
            observation.kind,
            EventKind::BiliCounts {
                seq: 7,
                room: 1,
                watched: Some(77),
                ..
            }
        ));
        let conn = crate::history::schema::open_rw(std::path::Path::new(":memory:")).unwrap();
        let mut writer = crate::history::writer::Writer::new(conn, "local", 1);
        let start = |seq| EventKind::SessionStarted {
            seq,
            platform: "YT",
            channel_id: "synthetic".into(),
            channel_name: "频道".into(),
            title: None,
            live_key: None,
            source_id: None,
            area_v2: Some(86),
            room: 1,
        };
        writer
            .commit(&[
                Event {
                    at: 100,
                    kind: start(7),
                },
                Event {
                    at: 105,
                    kind: EventKind::SessionEnded {
                        seq: 7,
                        reason: EndReason::SourceEnded,
                    },
                },
                Event {
                    at: 105,
                    kind: start(8),
                },
            ])
            .unwrap();
        writer.apply(&observation).unwrap();
        let (seq, room, watched): (u64, u64, f64) = writer.conn.query_row(
            "SELECT s.seq,s.room,r.max FROM samples_raw r JOIN sessions s ON r.session_id=s.id WHERE r.metric=11", [],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!((seq, room, watched), (7, 1, 77.0));
        writer
            .apply(&Event {
                at: 106,
                kind: EventKind::BiliCounts {
                    seq: 8,
                    room: 2,
                    online: Some(999),
                    watched: None,
                },
            })
            .unwrap();
        let n: u64 = writer
            .conn
            .query_row("SELECT count(*) FROM samples_raw", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            n, 1,
            "an observation of another room cannot attach to this session"
        );
    }

    #[test]
    fn room_warning_and_cut_packets_emit_safe_facts_without_running_commands() {
        use crate::history::EventKind;
        let client = client_for_messages();
        let bus = Arc::new(crate::events::Bus::new());
        let mut rx = bus.register("room-facts", 8, |_| true);
        bus.seal();
        let messages = [
            audience_message(
                "WARNING",
                serde_json::json!({"msg":"断开 rtmp://example.invalid/live?key=SYNTHETIC-RTMP"}),
            ),
            audience_message(
                "ANCHOR_ECOLOGY_LIVING_DIALOG",
                serde_json::json!({"dialog_title":"直播间违规"}),
            ),
        ];
        crate::events::with_sync_test_bus(bus, || {
            client.record_history_messages(&messages, Some(11), 123, Instant::now(), 5)
        });
        let warning = rx.try_recv().unwrap();
        let cut = rx.try_recv().unwrap();
        assert!(
            matches!(&warning.kind, EventKind::BiliWarning { seq: Some(11), room: 1, msg } if !msg.contains("SYNTHETIC-RTMP"))
        );
        assert!(
            matches!(&cut.kind, EventKind::BiliCutOff { seq: Some(11), room: 1, msg } if msg=="直播间违规")
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn decorative_and_incomplete_messages_log_only_safe_debug_types() {
        let client = client_for_messages();
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
        for cmd in [
            "SEND_GIFT_V2",
            "GIFT_POPUP",
            "SYS_MSG",
            "ROOM_CONTENT_AUDIT_REPORT",
        ] {
            let message = DanmakuMessage {
                cmd: cmd.into(),
                info: Some(serde_json::json!({"raw_body": "SYNTHETIC-UNSAFE-BODY"})),
                data: (cmd != "ROOM_CONTENT_AUDIT_REPORT")
                    .then(|| serde_json::json!({"body": "SYNTHETIC-UNSAFE-BODY"})),
            };
            client
                .process_danmaku_command(&message)
                .with_subscriber(dispatch.clone())
                .await;
        }
        let output = logs.text();
        assert_eq!(output.lines().count(), 4, "{output}");
        assert!(output.lines().all(|line| line.contains(" DEBUG [danmaku]")));
        assert!(!output.contains("SYNTHETIC-UNSAFE-BODY"));
        assert!(!output.contains(" WARN ["));
    }

    #[tokio::test]
    async fn a_disabled_non_owner_command_stays_rejected_before_command_processing() {
        let client = client_for_messages();
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
        let message = DanmakuMessage {
            cmd: "DANMU_MSG".into(),
            info: Some(serde_json::json!([
                [],
                "%转播%YT%示例频道001%其他单机",
                [2, "示例用户001"]
            ])),
            data: None,
        };
        client
            .process_danmaku_command(&message)
            .with_subscriber(dispatch)
            .await;
        let output = logs.text();
        assert_eq!(output.lines().count(), 1, "{output}");
        assert!(output.contains(" WARN [danmaku]"));
        assert!(output.contains("换台命令未启用"));
        assert!(!output.contains("监听目标已更新"));
        assert!(output.contains("弹幕 %转播%"));
        assert!(!output.contains("示例用户001"));
        assert!(!client.enable_commands.load(Ordering::Relaxed));
    }

    #[test]
    fn warning_followed_by_pending_room_lock_keeps_the_confirmed_stop_eligible() {
        use crate::plugins::danmaku::warning::BroadcastSource;
        let warnings = Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "示例频道 001", "UCexample001", Some("episode-1"));
        let _owner = warnings.own_source(1, source.clone());
        let mut frame = packet(0, br#"{"cmd":"WARNING","data":{"msg":"warning"}}"#);
        frame.extend(packet(0, br#"{"cmd":"ROOM_LOCK","data":{}}"#));
        let messages = decode_danmaku_messages(&frame).unwrap();
        let captured: Vec<_> = messages
            .iter()
            .map(|message| capture_warning_message(&warnings, message).unwrap())
            .collect();
        assert!(warnings.snapshot().holds(&source));
        assert!(
            warnings.event_is_current(&captured[0]),
            "unconfirmed ROOM_LOCK must not suppress WARNING stop"
        );
        warnings.finish_unmarked_event(&captured[1]);
        assert!(warnings.event_is_current(&captured[0]));
    }

    #[test]
    fn an_unknown_warning_source_finishes_after_failed_title_attribution() {
        use crate::plugins::danmaku::warning::{BroadcastSource, SourceObservation};
        let warnings = Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "示例频道 001", "UCexample001", Some("episode-1"));
        let owner = warnings.own_source(1, source.clone());
        warnings
            .mark_event(&mut warnings.capture_event(), None)
            .unwrap();
        drop(owner);
        let packet = DanmakuMessage {
            cmd: "CUT_OFF".into(),
            info: None,
            data: None,
        };
        let mut captured = capture_warning_message(&warnings, &packet).unwrap();
        assert!(!captured.has_source());
        assert!(warnings
            .mark_event(&mut captured, Some("a room title without relay identity"))
            .is_none());
        assert!(
            warnings.event_is_current(&captured),
            "failed attribution still permits the real warning stop"
        );
        warnings.finish_unmarked_event(&captured);
        let fresh = warnings.snapshot();
        assert!(
            warnings
                .observe_sources(&fresh, &[SourceObservation::new(source, false, false)])
                .is_some(),
            "a completed unknown warning must not leave pending_event latched"
        );
    }

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
