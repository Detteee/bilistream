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
    if let Some(line) = violation_log_line(message) {
        warn!("{}", line);
    }
    if cmd != "ROOM_LOCK" {
        let mut keys: Vec<String> = message
            .data
            .as_ref()
            .and_then(Value::as_object)
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        let reason = violation_reason(message).map(|reason| reason.reason);
        crate::history::capture::warning_frame(cmd, &keys, reason.as_deref());
    }
    let mut event = warnings.capture_event();
    // Classify the 违规原因 once, at capture, so every later mark of this
    // frame (including the stop task's) carries the same classes.
    event.set_reason_ids(
        violation_reason(message)
            .map(|reason| crate::plugins::warning_exempt::classify(&reason.reason))
            .unwrap_or_default(),
    );
    if cmd != "ROOM_LOCK" {
        if let Some(name) = warnings.mark_event(&mut event, None) {
            log_warning_mark(&name);
        }
    }
    Some(event)
}

/// The warn line for a 警告/切断 frame: Bilibili's cleaned, capped reason
/// (R3/R3b), or the existing fixed text. `ROOM_LOCK` logs elsewhere.
fn violation_log_line(message: &DanmakuMessage) -> Option<String> {
    let reason = || violation_reason(message).map(|reason| reason.text());
    match message.cmd.as_str() {
        "WARNING" => Some(format!(
            "Warning: {}",
            reason().as_deref().unwrap_or("No message")
        )),
        "FULL_SCREEN_MASK_OPEN" => Some(format!(
            "Cut off/Warning: {}",
            mask_title(message)
                .map(|title| safe_text(&title, REASON_COLUMNS))
                .as_deref()
                .unwrap_or("Stream cut off")
        )),
        "CUT_OFF" | "CUT_OFF_V2" | "ANCHOR_ECOLOGY_LIVING_DIALOG" => Some(format!(
            "Cut off/Warning: {}",
            reason().as_deref().unwrap_or("Stream cut off")
        )),
        _ => None,
    }
}

/// Complete title-only fallback while retaining the source captured at receipt.
/// No delayed response may attach itself to a replacement ffmpeg session.
/// The one danmaku line for this event. A released hold (the exemption lets
/// the source restart) takes the continues copy; otherwise the held copy.
fn violation_notice_for(cfg: &Config, warnings: &WarningTracker, warning: &WarningEvent) -> String {
    let snapshot = warnings.snapshot();
    let source = warning.source();
    let released = source.is_some_and(|source| {
        crate::plugins::danmaku::exempt_hold_release(cfg, &snapshot, source).is_some()
    });
    let channel_name = source
        .map(|source| source.channel_name.as_str())
        .unwrap_or_default();
    crate::plugins::violation_notice::text(warning.reason_ids(), released, channel_name)
}

/// A `msg/send` reply is accepted only with an empty `message`: Bilibili
/// answers `code 0` with a non-empty message for a filtered/repeat danmaku.
fn danmaku_reply_accepted(reply: &Value) -> bool {
    reply["message"]
        .as_str()
        .is_none_or(|message| message.trim().is_empty())
}

/// Send the chosen owner line. On a rejection (non-zero code, `code 0` with a
/// non-empty message such as a filtered repeat, or a transport error) send
/// today's generic text once as the last attempt; two sends max, one second
/// apart, and both failures return the last diagnostic.
async fn send_violation_notice(cfg: &Config, text: &str) -> Result<(), String> {
    let mut attempts: Vec<&str> = vec![text];
    if text != crate::plugins::violation_notice::GENERIC_SHORT {
        attempts.push(crate::plugins::violation_notice::GENERIC_SHORT);
    }
    let mut last = String::new();
    for (attempt, text) in attempts.into_iter().enumerate() {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match send_danmaku(cfg, text).await {
            Ok(reply) if danmaku_reply_accepted(&reply) => return Ok(()),
            Ok(reply) => {
                last = format!("code 0 but message={:?}", reply["message"]);
            }
            Err(error) => last = error.to_string(),
        }
    }
    Err(last)
}

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
    /// Top-level `msg`: the reason text of a real `WARNING` / `CUT_OFF`.
    #[serde(default)]
    pub msg: Option<Value>,
}

/// Exact labels of the `{type, label, content}` rows in `CUT_OFF_V2` and
/// `ANCHOR_ECOLOGY_LIVING_DIALOG`. Rows are found by label, never position.
const REASON_LABEL: &str = "违规原因";
const RESULT_LABEL: &str = "处罚结果";
/// The `safe_text` cap of the event `detail`, the log line and the QQ line.
const REASON_COLUMNS: usize = 160;

/// Where a [`Reason`] was read from, least to most detailed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasonSource {
    /// `ANCHOR_ECOLOGY_LIVING_DIALOG`'s `dialog_title` (last fallback).
    Title,
    /// Top-level `msg`, else `data.msg`.
    Message,
    /// The row labelled 违规原因.
    Labelled,
}

/// Bilibili's own reason for a 警告/切断 (PRD R1). Both texts are cleaned
/// (R3b) and non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reason {
    /// 处罚结果, e.g. 切断本场直播 / 警告.
    pub result: Option<String>,
    /// 违规原因, or the plain `msg`.
    pub reason: String,
    pub source: ReasonSource,
}

impl Reason {
    /// 「{违规原因}（{处罚结果}）」, else the reason alone; capped.
    pub(crate) fn text(&self) -> String {
        let full = match &self.result {
            Some(result) => format!("{}（{}）", self.reason, result),
            None => self.reason.clone(),
        };
        safe_text(&full, REASON_COLUMNS)
    }

    /// See [`crate::events::EndDetail::rank`].
    pub(crate) fn rank(&self) -> u8 {
        let source = match self.source {
            ReasonSource::Title => 1,
            ReasonSource::Message => 2,
            ReasonSource::Labelled => 3,
        };
        source * 2 + u8::from(self.result.is_some())
    }

    pub(crate) fn end_detail(&self) -> crate::events::EndDetail {
        crate::events::EndDetail {
            text: self.text(),
            rank: self.rank(),
        }
    }
}

/// R3b: Bilibili text is untrusted. Newlines, other control and invisible
/// format characters become one space, whitespace runs collapse, links are
/// dropped. The cap is applied afterwards by the caller.
fn clean_bili_text(raw: &str) -> String {
    let mut words = String::with_capacity(raw.len());
    let mut gap = false;
    for ch in raw.chars() {
        if matches!(ch, '\u{00ad}' | '\u{061c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}')
        {
            continue;
        }
        if ch.is_whitespace() || ch.is_control() {
            gap = !words.is_empty();
            continue;
        }
        if gap {
            words.push(' ');
            gap = false;
        }
        words.push(ch);
    }
    without_links(&words)
}

/// Drop every link (R3, Clerk L1/L1a/L1b): any `://` — ASCII, full-width
/// `：／／`, or a scheme with no ASCII letters such as `看://` — plus `www.…`
/// and a bare host: `host.tld`, `host:port`, `1.2.3.4[:port]`, with an
/// optional path, and with full-width or ideographic dots. A link runs to the
/// next whitespace or CJK punctuation, so a non-ASCII path is dropped with it.
fn without_links(text: &str) -> String {
    // Compare on a normalised copy so full-width `：／／` and full-width or
    // ideographic dots read as link punctuation. Only the link is replaced;
    // every other character is emitted exactly as Bilibili sent it.
    let orig: Vec<char> = text.chars().collect();
    let norm: Vec<char> = orig
        .iter()
        .map(|c| match c {
            '：' => ':',
            '／' => '/',
            '．' | '｡' | '。' => '.',
            _ => *c,
        })
        .collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < norm.len() {
        let token_start = i == 0 || !is_token_char(orig[i - 1]);
        if token_start {
            if let Some(end) = link_from_token(&orig, &norm, i) {
                out.push_str("[链接已省略]");
                i = end;
                continue;
            }
        } else if norm[i] == ':' && norm.get(i + 1) == Some(&'/') && norm.get(i + 2) == Some(&'/') {
            // L1a: a `://` later in a token still removes the link; the text
            // already emitted before the colon stays.
            out.push_str("[链接已省略]");
            i = link_body_end(&orig, &norm, i + 3);
            continue;
        }
        out.push(orig[i]);
        i += 1;
    }
    let collapsed: Vec<&str> = out.split(' ').filter(|w| !w.is_empty()).collect();
    collapsed.join(" ")
}

/// The link starting at token-start `i`, if any, and the index after it.
fn link_from_token(orig: &[char], norm: &[char], i: usize) -> Option<usize> {
    let at = |k: usize| norm.get(k).copied();
    // Scheme.run:// (ASCII, full-width, or no scheme at all — L1a).
    let mut scheme_end = i;
    while at(scheme_end).is_some_and(scheme_char) {
        scheme_end += 1;
    }
    let scheme = at(i).is_some_and(char::is_alphanumeric);
    if (scheme || scheme_end == i)
        && at(scheme_end) == Some(':')
        && at(scheme_end + 1) == Some('/')
        && at(scheme_end + 2) == Some('/')
    {
        return Some(link_body_end(orig, norm, scheme_end + 3));
    }
    host_link_end(orig, norm, i)
}

/// The index after a link body: spaces after `://` belong to the link, then
/// the body runs to whitespace or CJK punctuation.
fn link_body_end(orig: &[char], norm: &[char], mut body: usize) -> usize {
    while norm.get(body) == Some(&' ') {
        body += 1;
    }
    link_end(orig, norm, body)
}

/// L1b: a bare host at token-start `i` — `www.…`, `host.tld`, `host:port`,
/// `1.2.3.4[:port]`, with an optional path, and with full-width or
/// ideographic dots. Dotted versions and words without a host-shaped last
/// label are not links.
fn host_link_end(orig: &[char], norm: &[char], i: usize) -> Option<usize> {
    let at = |k: usize| norm.get(k).copied();
    if starts_with_www(norm, i) {
        return Some(link_end(orig, norm, i));
    }
    let mut k = i;
    let mut labels = 0usize;
    let mut last_alpha;
    let mut all_numeric = true;
    loop {
        let start = k;
        while at(k).is_some_and(host_char) {
            k += 1;
        }
        if k == start {
            return None;
        }
        let label = &norm[start..k];
        labels += 1;
        last_alpha = if label.iter().all(|c| c.is_ascii_alphabetic()) {
            label.len()
        } else {
            0
        };
        all_numeric &= label.iter().all(|c| c.is_ascii_digit());
        if at(k) == Some('.') && at(k + 1).is_some_and(host_char) {
            k += 1;
        } else {
            break;
        }
    }
    if labels < 2 {
        return None;
    }
    let ip = labels == 4 && all_numeric;
    // host.tld: an alphabetic last label of two or more letters.
    let domain = !all_numeric && last_alpha >= 2;
    if !ip && !domain {
        return None;
    }
    // Optional `:port` (host:port / IP:port).
    if at(k) == Some(':') && at(k + 1).is_some_and(|c| c.is_ascii_digit()) {
        k += 1;
        while at(k).is_some_and(|c| c.is_ascii_digit()) {
            k += 1;
        }
    }
    if at(k) == Some('/') {
        return Some(link_end(orig, norm, k));
    }
    // A bare host ends where its labels (and port) stop; running Chinese text
    // after it does not keep the host visible.
    Some(k)
}

/// `www.` in any ASCII letter case.
fn starts_with_www(norm: &[char], i: usize) -> bool {
    ['w', 'w', 'w', '.']
        .into_iter()
        .enumerate()
        .all(|(offset, expected)| {
            norm.get(i + offset)
                .is_some_and(|c| c.to_ascii_lowercase() == expected)
        })
}

/// ASCII or full-width letters/digits/`+`/`-`/`.`, i.e. URL scheme material.
fn scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '+' | '-' | '.')
        || matches!(
            c,
            '\u{ff10}'..='\u{ff19}' | '\u{ff21}'..='\u{ff3a}' | '\u{ff41}'..='\u{ff5a}'
        )
}

/// ASCII letters/digits/`-`, i.e. host label material.
fn host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// A character that continues a token, so a link cannot start mid-word.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// The CJK punctuation that ends a link body and a token.
fn is_cjk_punct(c: char) -> bool {
    matches!(
        c,
        '\u{3000}'
            ..='\u{303f}'
                | '\u{ff01}'
                | '\u{ff08}'
                | '\u{ff09}'
                | '\u{ff0c}'
                | '\u{ff1a}'
                | '\u{ff1b}'
                | '\u{ff1f}'
    )
}

/// The index after a link body: the next whitespace or CJK punctuation.
fn link_end(orig: &[char], norm: &[char], mut k: usize) -> usize {
    while let Some(&c) = norm.get(k) {
        // Judge punctuation on the original character: a full-width `：` was
        // only normalised for matching and still ends a body.
        if c.is_whitespace() || orig.get(k).is_some_and(|o| is_cjk_punct(*o)) {
            // `。` is a host dot only when a whole host label follows it
            // (`https://b23。tv`), never when prose follows (`x.com。B站官方`).
            if orig.get(k) == Some(&'。') {
                let mut next = k + 1;
                while norm.get(next).copied().is_some_and(host_char) {
                    next += 1;
                }
                let label_ends = next == norm.len()
                    || norm[next].is_whitespace()
                    || matches!(orig.get(next), Some(&'/') | Some(&':'))
                    || orig.get(next).is_some_and(|o| is_cjk_punct(*o));
                if next > k + 1 && label_ends {
                    k = next;
                    continue;
                }
            }
            break;
        }
        k += 1;
    }
    k
}

/// The `content` of the first row whose `label` is exactly `label` and whose
/// content is a non-empty string after cleaning. Absent label: `None`.
fn labelled_row(rows: Option<&Value>, label: &str) -> Option<String> {
    rows?
        .as_array()?
        .iter()
        .filter(|row| row.get("label").and_then(Value::as_str) == Some(label))
        .filter_map(|row| row.get("content").and_then(Value::as_str))
        .map(clean_bili_text)
        .find(|text| !text.is_empty())
}

/// `FULL_SCREEN_MASK_OPEN`'s `data.title`, cleaned (R3b); nothing else of
/// the frame is read.
fn mask_title(message: &DanmakuMessage) -> Option<String> {
    cleaned(message.data.as_ref().and_then(|data| data.get("title")))
}

fn cleaned(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(clean_bili_text)
        .filter(|text| !text.is_empty())
}

/// PRD R1: the one place a 警告/切断 reason is read. The labelled 违规原因 row
/// wins, then top-level `msg`, then `data.msg`, then (dialog only) its
/// `dialog_title`. `None` keeps the caller's fixed fallback text.
pub(crate) fn violation_reason(message: &DanmakuMessage) -> Option<Reason> {
    let data = message.data.as_ref();
    let rows = match message.cmd.as_str() {
        "CUT_OFF_V2" => data
            .and_then(|data| data.get("cut_off_data"))
            .and_then(|cut| cut.get("cut_off_message_list")),
        "ANCHOR_ECOLOGY_LIVING_DIALOG" => data.and_then(|data| data.get("dialog_message_list")),
        "WARNING" | "CUT_OFF" => None,
        _ => return None,
    };
    let result = labelled_row(rows, RESULT_LABEL);
    let (reason, source) = labelled_row(rows, REASON_LABEL)
        .map(|reason| (reason, ReasonSource::Labelled))
        .or_else(|| {
            cleaned(message.msg.as_ref())
                .or_else(|| cleaned(data.and_then(|data| data.get("msg"))))
                .map(|reason| (reason, ReasonSource::Message))
        })
        .or_else(|| {
            cleaned(data.and_then(|data| data.get("dialog_title")))
                .filter(|_| message.cmd == "ANCHOR_ECOLOGY_LIVING_DIALOG")
                .map(|title| (title, ReasonSource::Title))
        })?;
    Some(Reason {
        result,
        reason,
        source,
    })
}

/// The newest Bilibili reason per room, for the 下播 notice's 结束原因 line.
/// Every frame of one 警告/切断 arrives within [`VIOLATION_WINDOW`]; within
/// it a more detailed reason replaces a less detailed one, never the reverse.
const VIOLATION_WINDOW: Duration =
    Duration::from_secs(crate::history::VIOLATION_WINDOW_SECS as u64);

fn recent_violations(
) -> &'static Mutex<std::collections::HashMap<u64, (Instant, crate::events::EndDetail)>> {
    static RECENT: std::sync::OnceLock<
        Mutex<std::collections::HashMap<u64, (Instant, crate::events::EndDetail)>>,
    > = std::sync::OnceLock::new();
    RECENT.get_or_init(Default::default)
}

fn note_violation(room: u64, detail: crate::events::EndDetail, now: Instant) {
    let mut recent = recent_violations()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match recent.get_mut(&room) {
        Some((first, kept)) if now.saturating_duration_since(*first) <= VIOLATION_WINDOW => {
            if detail.rank > kept.rank {
                *kept = detail;
            }
        }
        _ => {
            recent.insert(room, (now, detail));
        }
    }
}

/// The best reason seen for `room` in the current window, if any. The stop
/// call runs before the 下播 signal, so a reason stays usable for two windows
/// after its first frame.
pub(crate) fn recent_violation(room: u64, now: Instant) -> Option<crate::events::EndDetail> {
    let recent = recent_violations()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    recent
        .get(&room)
        .filter(|(first, _)| now.saturating_duration_since(*first) <= VIOLATION_WINDOW * 2)
        .map(|(_, detail)| detail.clone())
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

/// The fixed event `detail` when Bilibili sent no reason. The history query
/// treats every one as "no reason" (`history::STOCK_FALLBACKS`).
const FALLBACK_WARNING: &str = crate::history::STOCK_FALLBACKS[0];
const FALLBACK_CUT_OFF: &str = crate::history::STOCK_FALLBACKS[1];
const FALLBACK_DIALOG: &str = crate::history::STOCK_FALLBACKS[2];
const FALLBACK_MASK: &str = crate::history::STOCK_FALLBACKS[3];

/// How a 警告/切断 frame stops the room. Classification (ruling 4) never
/// changes this: a mask is recorded as a warning but still stops the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopOrder {
    /// `WARNING`: `bili_stop_live` first, then the 换台 danmaku.
    StopFirst,
    /// The 换台 danmaku first, then `bili_stop_live`.
    DanmakuFirst,
}

fn violation_stop(cmd: &str) -> Option<StopOrder> {
    match cmd {
        "WARNING" => Some(StopOrder::StopFirst),
        "CUT_OFF" | "CUT_OFF_V2" | "ANCHOR_ECOLOGY_LIVING_DIALOG" | "FULL_SCREEN_MASK_OPEN" => {
            Some(StopOrder::DanmakuFirst)
        }
        _ => None,
    }
}

/// PRD ruling 4: which history kind a 警告/切断 frame is recorded as.
/// `WARNING` and the timed `FULL_SCREEN_MASK_OPEN` cover are `bili.warning`;
/// `CUT_OFF` and `CUT_OFF_V2` are `bili.cut_off`. The warning dialog
/// `ANCHOR_ECOLOGY_LIVING_DIALOG` is a `bili.warning` unless its 处罚结果 row
/// (exact label) contains 切断. Rows already stored are never rewritten.
fn records_as_cut_off(message: &DanmakuMessage) -> bool {
    match message.cmd.as_str() {
        "WARNING" | "FULL_SCREEN_MASK_OPEN" => false,
        "ANCHOR_ECOLOGY_LIVING_DIALOG" => labelled_row(
            message
                .data
                .as_ref()
                .and_then(|data| data.get("dialog_message_list")),
            RESULT_LABEL,
        )
        .is_some_and(|result| result.contains("切断")),
        _ => true,
    }
}

fn room_history_event(
    message: &DanmakuMessage,
    room: u64,
    seq: Option<u64>,
) -> Option<crate::history::EventKind> {
    use crate::history::EventKind;
    let (msg, rank) = match message.cmd.as_str() {
        // Ruling 4: the mask's detail is `data.title` only (never `lines`,
        // `icon` or `room_id`), cleaned like every reason.
        "FULL_SCREEN_MASK_OPEN" => match mask_title(message) {
            Some(title) => (safe_text(&title, REASON_COLUMNS), 2),
            None => (FALLBACK_MASK.to_string(), 0),
        },
        cmd => {
            let fallback = match cmd {
                "WARNING" => FALLBACK_WARNING,
                "CUT_OFF" | "CUT_OFF_V2" => FALLBACK_CUT_OFF,
                "ANCHOR_ECOLOGY_LIVING_DIALOG" => FALLBACK_DIALOG,
                _ => return None,
            };
            match violation_reason(message) {
                Some(reason) => (reason.text(), reason.rank()),
                None => (fallback.to_string(), 0),
            }
        }
    };
    // PRD 65e7118b / Clerk L4: only CUT_OFF with CUT_OFF_V2 merge (and an
    // identical repeated WARNING is dropped); every other frame keeps its row.
    let merge = matches!(message.cmd.as_str(), "CUT_OFF" | "CUT_OFF_V2" | "WARNING");
    // Ruling: a warning that stops the room is stored as `bili.warning.stop`,
    // and the session then reads `end_warning_stop`. Every warning frame we
    // know stops the room.
    let stop = violation_stop(&message.cmd).is_some();
    Some(if !records_as_cut_off(message) {
        EventKind::BiliWarning {
            seq,
            room,
            msg,
            rank,
            merge,
            stop,
        }
    } else {
        EventKind::BiliCutOff {
            seq,
            room,
            msg,
            rank,
            merge,
        }
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
                if let Some(reason) = violation_reason(message) {
                    note_violation(self.room_id, reason.end_detail(), observed_at);
                }
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
            cmd if violation_stop(cmd) == Some(StopOrder::StopFirst) => {
                let cfg = self.app_config.clone();
                let room = self.room_id;
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
                            Ok(()) => crate::notify::live_ended_with(
                                crate::events::BroadcastEndCause::WarningCut,
                                recent_violation(room, Instant::now()),
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
                        let notice = violation_notice_for(&cfg, &warning_tracker(), &warning);
                        if let Err(error) = send_violation_notice(&cfg, &notice).await {
                            error!(
                                "Failed to send warning danmaku: {}",
                                safe_diagnostic(&error)
                            );
                        }
                        warning_tracker().finish_unmarked_event(&warning);
                    });
                }
            }
            cmd if violation_stop(cmd) == Some(StopOrder::DanmakuFirst) => {
                let cfg = self.app_config.clone();
                let room = self.room_id;
                if let Some(warning) = warning {
                    tokio::spawn(async move {
                        let warning =
                            complete_warning_event(warning, cfg.bililive.room, false).await;
                        if !warning_tracker().event_is_current(&warning) {
                            return;
                        }
                        let notice = violation_notice_for(&cfg, &warning_tracker(), &warning);
                        if let Err(error) = send_violation_notice(&cfg, &notice).await {
                            error!(
                                "Failed to send warning danmaku: {}",
                                safe_diagnostic(&error)
                            );
                        }
                        if !warning_tracker().event_is_current(&warning) {
                            return;
                        }
                        match bili_stop_live(&cfg).await {
                            Ok(()) => crate::notify::live_ended_with(
                                crate::events::BroadcastEndCause::WarningCut,
                                recent_violation(room, Instant::now()),
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
            msg: None,
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
        let dialog = rx.try_recv().unwrap();
        assert!(
            matches!(&warning.kind, EventKind::BiliWarning { seq: Some(11), room: 1, msg, rank: 4, merge: true, stop: true } if !msg.contains("SYNTHETIC-RTMP") && !msg.contains("rtmp://"))
        );
        // Ruling 4: a dialog without a 切断 处罚结果 is a warning.
        assert!(
            matches!(&dialog.kind, EventKind::BiliWarning { seq: Some(11), room: 1, msg, rank: 2, merge: false, stop: true } if msg=="直播间违规")
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
                msg: None,
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
            msg: None,
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
            msg: None,
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

    // --------------------------------------- cutoff-reason (PRD R1–R3b)

    /// Real payloads from the research report (bilibili-API-collect mirror,
    /// EricEcho/BilibiliTool), public captures with no user data.
    const FIX_WARNING_1: &str =
        r#"{"cmd":"WARNING","msg":"图片内容不适宜，请立即调整","roomid":22195814}"#;
    const FIX_WARNING_2: &str =
        r#"{"cmd":"WARNING","msg":"禁止直播违禁游戏，请立即更换","roomid":776023}"#;
    const FIX_CUT_OFF: &str =
        r#"{"cmd":"CUT_OFF","msg":"违反直播言论规范，请立即调整","roomid":23993070}"#;
    /// CUT_OFF_V2 capture of 2024-11-14 as trimmed in the report.
    const FIX_CUT_OFF_V2: &str = r#"{"cmd":"CUT_OFF_V2","data":{"cut_off_scene":1,"timestamp":1731590280,"cut_off_version":1,
 "cut_off_data":{"cut_off_title":"违规提示","cut_off_message_list":[
  {"type":1,"label":"处罚结果","content":"切断本场直播"},
  {"type":1,"label":"违规原因","content":"您本场直播存在挂机、录播等消极直播行为，因此直播被切断，请您及时整改"},
  {"type":1,"label":"处罚时间","content":"2024年11月14日21时17分"}],
 "cut_off_tip_list":[{"show_platform":[3,4],"message_list":[{"type":1,"content":"请在","link_url":""},
  {"type":2,"content":"【处罚中心】","link_url":"https://link.bilibili.com/p/center/index?my-room/violation-records#/my-room/violation-records"},
  {"type":1,"content":"查看你的违规记录","link_url":""}]}],
 "cut_off_button_list":[{"show_platform":[1,2],"button_text":"了解详情","button_action":2,
  "button_link_url":"https://live.bilibili.com/p/html/live-anchor-galaxy/violation_records/mobile.html?-Abrowser=live&is_live_webview=1"},
  {"show_platform":[3,4],"button_text":"我知道了","button_action":1,"button_link_url":""}]}}}"#;
    /// ANCHOR_ECOLOGY_LIVING_DIALOG: the report's 2024-11-13 rows inside the
    /// documented envelope (tip and button lists as in CUT_OFF_V2).
    const FIX_DIALOG: &str = r#"{"cmd":"ANCHOR_ECOLOGY_LIVING_DIALOG","data":{"dialog_scene":1,"timestamp":1731504840,"valid_timestamp":1731505840,
 "dialog_title":"直播间违规","dialog_message_list":[
  {"type":1,"label":"处罚结果","content":"警告"},
  {"type":1,"label":"违规原因","content":"您本场直播存在挂机、录播等消极直播行为，请及时整改"},
  {"type":1,"label":"处罚时间","content":"2024年11月13日21时34分"}],
 "dialog_tip_list":[{"show_platform":[3,4],"message_list":[{"type":2,"content":"【处罚中心】","link_url":"https://link.bilibili.com/p/center/index#/my-room/violation-records"}]}],
 "dialog_button_list":[{"show_platform":[1,2],"button_text":"了解详情","button_action":2,"button_link_url":"https://live.bilibili.com/p/html/live-anchor-galaxy/violation_records/mobile.html"}]}}"#;

    const V2_EXPECTED: &str =
        "您本场直播存在挂机、录播等消极直播行为，因此直播被切断，请您及时整改（切断本场直播）";
    const DIALOG_EXPECTED: &str = "您本场直播存在挂机、录播等消极直播行为，请及时整改（警告）";

    fn parse(json: &str) -> DanmakuMessage {
        serde_json::from_str(json).unwrap()
    }

    fn history(message: &DanmakuMessage) -> (bool, String, u8) {
        use crate::history::EventKind;
        match room_history_event(message, 1, None).unwrap() {
            EventKind::BiliWarning { msg, rank, .. } => (false, msg, rank),
            EventKind::BiliCutOff { msg, rank, .. } => (true, msg, rank),
            _ => unreachable!(),
        }
    }

    fn v2_with_rows(rows: Value) -> DanmakuMessage {
        audience_message(
            "CUT_OFF_V2",
            serde_json::json!({"cut_off_data": {"cut_off_message_list": rows}}),
        )
    }

    #[test]
    fn real_payloads_give_the_exact_history_text() {
        for (json, cut, text, rank) in [
            (FIX_WARNING_1, false, "图片内容不适宜，请立即调整", 4),
            (FIX_WARNING_2, false, "禁止直播违禁游戏，请立即更换", 4),
            (FIX_CUT_OFF, true, "违反直播言论规范，请立即调整", 4),
            (FIX_CUT_OFF_V2, true, V2_EXPECTED, 7),
            (FIX_DIALOG, false, DIALOG_EXPECTED, 7),
        ] {
            let message = parse(json);
            assert_eq!(history(&message), (cut, text.to_string(), rank), "{json}");
            let reason = violation_reason(&message).unwrap();
            assert_eq!(reason.text(), text);
            assert_eq!(reason.end_detail().rank, rank);
        }
        let v2 = violation_reason(&parse(FIX_CUT_OFF_V2)).unwrap();
        assert_eq!(v2.result.as_deref(), Some("切断本场直播"));
        assert_eq!(v2.source, ReasonSource::Labelled);
    }

    #[test]
    fn rows_are_found_by_exact_label_in_any_order_with_extra_rows() {
        let rows = serde_json::json!([
            {"type":1,"label":"处罚时间","content":"2024年11月14日21时17分"},
            {"type":1,"label":"违规原因说明","content":"不是这一行"},
            {"type":1,"label":" 违规原因","content":"也不是这一行"},
            {"type":1,"label":"违规原因","content":"您本场直播存在挂机、录播等消极直播行为，因此直播被切断，请您及时整改"},
            {"type":2,"label":"备注","content":"额外行"},
            {"type":1,"label":"处罚结果","content":"切断本场直播"}
        ]);
        assert_eq!(history(&v2_with_rows(rows)), (true, V2_EXPECTED.into(), 7));
    }

    #[test]
    fn a_missing_label_leaves_that_field_out() {
        // No 违规原因 row: no reason, so the fixed fallback stays.
        let only_result = v2_with_rows(serde_json::json!([
            {"label":"处罚结果","content":"切断本场直播"},
            {"label":"处罚时间","content":"2024年11月14日21时17分"}
        ]));
        assert!(violation_reason(&only_result).is_none());
        assert_eq!(history(&only_result), (true, "直播间切断".into(), 0));
        // No 处罚结果 row: the reason alone, no brackets.
        let only_reason = v2_with_rows(serde_json::json!([
            {"label":"处罚时间","content":"2024年11月14日21时17分"},
            {"label":"违规原因","content":"存在挂机、录播等消极直播行为"}
        ]));
        assert_eq!(
            history(&only_reason),
            (true, "存在挂机、录播等消极直播行为".into(), 6)
        );
        // Row content is never taken by position.
        let unlabelled = v2_with_rows(serde_json::json!([
            {"content":"切断本场直播"},
            {"content":"存在挂机、录播等消极直播行为"}
        ]));
        assert_eq!(history(&unlabelled), (true, "直播间切断".into(), 0));
    }

    #[test]
    fn non_string_or_blank_content_is_ignored() {
        let rows = v2_with_rows(serde_json::json!([
            {"label":"违规原因","content":12345},
            {"label":"违规原因","content":" \n\t "},
            {"label":"违规原因","content":{"text":"对象"}},
            {"label":"违规原因","content":"真正的原因"},
            {"label":"处罚结果","content":["数组"]}
        ]));
        assert_eq!(history(&rows), (true, "真正的原因".into(), 6));
        let none = v2_with_rows(serde_json::json!([{"label":"违规原因","content":null}]));
        assert_eq!(history(&none), (true, "直播间切断".into(), 0));
        let not_a_list = audience_message(
            "CUT_OFF_V2",
            serde_json::json!({"cut_off_data": {"cut_off_message_list": "违规原因"}}),
        );
        assert_eq!(history(&not_a_list), (true, "直播间切断".into(), 0));
    }

    #[test]
    fn message_fallbacks_top_level_msg_then_data_msg_then_fixed_text() {
        // Only top-level msg (the real WARNING/CUT_OFF shape).
        assert_eq!(
            history(&parse(r#"{"cmd":"CUT_OFF","msg":"顶层原因"}"#)),
            (true, "顶层原因".into(), 4)
        );
        // Only data.msg: the old test shape still works.
        assert_eq!(
            history(&parse(r#"{"cmd":"WARNING","data":{"msg":"旧格式原因"}}"#)),
            (false, "旧格式原因".into(), 4)
        );
        // Both: the top-level one wins.
        assert_eq!(
            history(&parse(
                r#"{"cmd":"WARNING","msg":"顶层原因","data":{"msg":"旧格式原因"}}"#
            )),
            (false, "顶层原因".into(), 4)
        );
        // A non-string msg is ignored.
        assert_eq!(
            history(&parse(
                r#"{"cmd":"CUT_OFF","msg":{"a":1},"data":{"msg":7}}"#
            )),
            (true, "直播间切断".into(), 0)
        );
        // None at all: today's fallback text.
        assert_eq!(
            history(&parse(r#"{"cmd":"WARNING","roomid":1}"#)),
            (false, "直播间警告".into(), 0)
        );
        assert_eq!(
            history(&parse(r#"{"cmd":"CUT_OFF","roomid":1}"#)),
            (true, "直播间切断".into(), 0)
        );
        assert_eq!(
            history(&parse(r#"{"cmd":"CUT_OFF_V2","data":{}}"#)),
            (true, "直播间切断".into(), 0)
        );
        // The dialog's title is only the last fallback.
        assert_eq!(
            history(&parse(
                r#"{"cmd":"ANCHOR_ECOLOGY_LIVING_DIALOG","data":{"dialog_title":"直播间违规"}}"#
            )),
            (false, "直播间违规".into(), 2)
        );
        assert_eq!(
            history(&parse(
                r#"{"cmd":"ANCHOR_ECOLOGY_LIVING_DIALOG","data":{}}"#
            )),
            (false, "直播间违规".into(), 0)
        );
        // ROOM_LOCK keeps today's handling; a mask is a warning (ruling 4).
        assert!(
            room_history_event(&parse(r#"{"cmd":"ROOM_LOCK","expire":"x"}"#), 1, None).is_none()
        );
        assert!(violation_reason(&parse(r#"{"cmd":"ROOM_LOCK","msg":"x"}"#)).is_none());
        assert_eq!(
            history(&parse(
                r#"{"cmd":"FULL_SCREEN_MASK_OPEN","data":{"title":"遮罩标题"}}"#
            )),
            (false, "遮罩标题".into(), 2)
        );
    }

    #[test]
    fn control_characters_and_newlines_cannot_fake_lines_or_break_layout() {
        let message = audience_message(
            "WARNING",
            serde_json::json!({"msg": "违规\n2026-10-20T00:00:00 INFO fake\r\t\u{7}\u{202e}末尾  多\u{3000}空格\u{200b}"}),
        );
        let (_, msg, _) = history(&message);
        assert_eq!(msg, "违规 2026-10-20T00:00:00 INFO fake 末尾 多 空格");
        assert!(!msg.contains('\\'), "{msg}");
        let rows = v2_with_rows(serde_json::json!([
            {"label":"违规原因","content":"第一行\n第二行"},
            {"label":"处罚结果","content":"\u{1b}[31m切断\u{1b}[0m"}
        ]));
        let (_, msg, _) = history(&rows);
        assert!(!msg.chars().any(char::is_control), "{msg:?}");
        assert!(msg.starts_with("第一行 第二行（"), "{msg}");
    }

    #[test]
    fn over_cap_text_is_capped_after_cleaning() {
        let long = "很长".repeat(200);
        let message = audience_message("CUT_OFF", serde_json::json!({"msg": long}));
        let (_, msg, rank) = history(&message);
        assert_eq!(rank, 4);
        assert!(unicode_width::UnicodeWidthStr::width(msg.as_str()) <= REASON_COLUMNS);
        assert!(msg.starts_with("很长很长"));
        assert!(msg.chars().count() < long.chars().count());
        // Newlines are stripped before the cap, so they never cost columns.
        let spaced = audience_message(
            "CUT_OFF",
            serde_json::json!({"msg": format!("{}{}", "\n".repeat(500), "原因")}),
        );
        assert_eq!(history(&spaced).1, "原因");
    }

    #[test]
    fn links_in_the_payload_are_never_stored() {
        for json in [FIX_CUT_OFF_V2, FIX_DIALOG] {
            let (_, msg, _) = history(&parse(json));
            assert!(
                !msg.contains("http") && !msg.contains("bilibili.com"),
                "{msg}"
            );
            assert!(
                !msg.contains("1731590280") && !msg.contains("2024年"),
                "{msg}"
            );
        }
        let inline = audience_message(
            "CUT_OFF",
            serde_json::json!({"msg": "请查看 https://live.bilibili.com/p/x?a=1&b=2 处罚中心，或rtmp://h.example/live?key=SYNTHETIC-RTMP。"}),
        );
        let (_, msg, _) = history(&inline);
        assert_eq!(msg, "请查看 [链接已省略] 处罚中心，或[链接已省略]。");
    }

    #[test]
    fn the_warn_log_line_carries_the_reason_and_no_link() {
        for (json, line) in [
            (
                FIX_WARNING_1,
                "Warning: 图片内容不适宜，请立即调整".to_string(),
            ),
            (
                FIX_WARNING_2,
                "Warning: 禁止直播违禁游戏，请立即更换".to_string(),
            ),
            (
                FIX_CUT_OFF,
                "Cut off/Warning: 违反直播言论规范，请立即调整".to_string(),
            ),
            (FIX_CUT_OFF_V2, format!("Cut off/Warning: {V2_EXPECTED}")),
            (FIX_DIALOG, format!("Cut off/Warning: {DIALOG_EXPECTED}")),
            (r#"{"cmd":"WARNING"}"#, "Warning: No message".to_string()),
            (
                r#"{"cmd":"CUT_OFF_V2"}"#,
                "Cut off/Warning: Stream cut off".to_string(),
            ),
        ] {
            assert_eq!(
                violation_log_line(&parse(json)).as_deref(),
                Some(line.as_str())
            );
        }
        assert_eq!(violation_log_line(&parse(r#"{"cmd":"ROOM_LOCK"}"#)), None);
    }

    #[tokio::test]
    async fn the_captured_cut_log_contains_the_reason_and_no_url() {
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
        for json in [FIX_CUT_OFF_V2, FIX_DIALOG] {
            let message = parse(json);
            async { capture_warning_message(&warning_tracker(), &message) }
                .with_subscriber(dispatch.clone())
                .await;
        }
        let output = logs.text();
        assert!(output.contains(V2_EXPECTED), "{output}");
        assert!(output.contains(DIALOG_EXPECTED), "{output}");
        for leak in ["http", "bilibili.com", "link_url", "1731590280", "2024年"] {
            assert!(!output.contains(leak), "{leak}: {output}");
        }
    }

    #[test]
    fn the_notice_reason_keeps_the_most_detailed_frame_per_room() {
        let room = 9_100_001;
        let now = Instant::now();
        let plain = violation_reason(&parse(FIX_CUT_OFF)).unwrap().end_detail();
        let v2 = violation_reason(&parse(FIX_CUT_OFF_V2))
            .unwrap()
            .end_detail();
        // CUT_OFF then V2: upgraded.
        note_violation(room, plain.clone(), now);
        note_violation(room, v2.clone(), now + Duration::from_millis(5));
        assert_eq!(
            recent_violation(room, now + Duration::from_secs(1)),
            Some(v2.clone())
        );
        // A later plain msg never downgrades it.
        note_violation(room, plain.clone(), now + Duration::from_millis(10));
        assert_eq!(
            recent_violation(room, now + Duration::from_secs(1)),
            Some(v2.clone())
        );
        // V2 then CUT_OFF for another room.
        let other = room + 1;
        note_violation(other, v2.clone(), now);
        note_violation(other, plain.clone(), now);
        assert_eq!(recent_violation(other, now), Some(v2.clone()));
        // V2 alone; nothing for an untouched room; stale after the window.
        let alone = room + 2;
        note_violation(alone, v2.clone(), now);
        assert_eq!(recent_violation(alone, now), Some(v2));
        assert_eq!(recent_violation(room + 3, now), None);
        assert_eq!(
            recent_violation(alone, now + VIOLATION_WINDOW * 3),
            None,
            "an old cut is not this 下播's reason"
        );
        // A new cut after the window starts over, even when less detailed.
        note_violation(alone, plain.clone(), now + VIOLATION_WINDOW * 3);
        assert_eq!(
            recent_violation(alone, now + VIOLATION_WINDOW * 3),
            Some(plain)
        );
    }

    #[test]
    fn one_frame_with_cut_off_and_v2_emits_both_ranked_for_the_writer() {
        use crate::history::EventKind;
        let client = client_for_messages();
        let bus = Arc::new(crate::events::Bus::new());
        let mut rx = bus.register("cut-facts", 8, |_| true);
        bus.seal();
        let messages = [parse(FIX_CUT_OFF), parse(FIX_CUT_OFF_V2)];
        crate::events::with_sync_test_bus(bus, || {
            client.record_history_messages(&messages, Some(3), 500, Instant::now(), 5)
        });
        let first = rx.try_recv().unwrap();
        let second = rx.try_recv().unwrap();
        assert!(
            matches!(&first.kind, EventKind::BiliCutOff { rank: 4, msg, .. } if msg == "违反直播言论规范，请立即调整")
        );
        assert!(
            matches!(&second.kind, EventKind::BiliCutOff { rank: 7, msg, .. } if msg == V2_EXPECTED)
        );
        assert!(rx.try_recv().is_err());
    }

    // ------------------------------------------- PRD ruling 4 (kinds)

    #[test]
    fn ruling4_the_2024_11_13_warning_dialog_is_recorded_as_bili_warning() {
        use crate::history::EventKind;
        let message = parse(FIX_DIALOG);
        assert!(!records_as_cut_off(&message));
        assert!(matches!(
            room_history_event(&message, 1, Some(2)).unwrap(),
            EventKind::BiliWarning { seq: Some(2), room: 1, ref msg, rank: 7, merge: false, stop: true } if msg == DIALOG_EXPECTED
        ));
    }

    #[test]
    fn ruling4_a_dialog_whose_result_contains_cut_is_recorded_as_bili_cut_off() {
        use crate::history::EventKind;
        // Synthetic: the dialog shape with a 切断 处罚结果, rows reordered.
        let message = parse(
            r#"{"cmd":"ANCHOR_ECOLOGY_LIVING_DIALOG","data":{"dialog_title":"直播间违规","dialog_message_list":[
              {"type":1,"label":"违规原因","content":"您本场直播存在挂机、录播等消极直播行为"},
              {"type":1,"label":"处罚时间","content":"2026年10月20日19时00分"},
              {"type":1,"label":"处罚结果","content":"切断本场直播"}]}}"#,
        );
        assert!(records_as_cut_off(&message));
        assert!(matches!(
            room_history_event(&message, 1, None).unwrap(),
            EventKind::BiliCutOff { ref msg, rank: 7, .. }
                if msg == "您本场直播存在挂机、录播等消极直播行为（切断本场直播）"
        ));
        // 切断 decides only through the exact 处罚结果 label.
        let elsewhere = parse(
            r#"{"cmd":"ANCHOR_ECOLOGY_LIVING_DIALOG","data":{"dialog_title":"切断","dialog_message_list":[
              {"label":"违规原因","content":"可能被切断"},
              {"label":"处罚结果说明","content":"切断"},
              {"label":"处罚结果","content":"警告"}]}}"#,
        );
        assert!(!records_as_cut_off(&elsewhere));
        // A 切断 result with no 违规原因 is still a cut, with the fallback text.
        let no_reason = parse(
            r#"{"cmd":"ANCHOR_ECOLOGY_LIVING_DIALOG","data":{"dialog_message_list":[
              {"label":"处罚结果","content":"切断本场直播"}]}}"#,
        );
        assert!(matches!(
            room_history_event(&no_reason, 1, None).unwrap(),
            EventKind::BiliCutOff { ref msg, rank: 0, .. } if msg == "直播间违规"
        ));
    }

    /// FULL_SCREEN_MASK_OPEN in the shape of the sjh8130/bili_danmaku capture
    /// (room id and image host replaced with synthetic values).
    const FIX_MASK: &str = r#"{"cmd":"FULL_SCREEN_MASK_OPEN","data":{"has_mask":true,"icon":"https://i0.example.invalid/bfs/live/mask.png","lines":"主播及时更改，可继续直播，若继续违规，直播间将被关闭。","overlay_second":60,"room_id":9999999999,"title":"当前直播间涉嫌违规"}}"#;

    #[test]
    fn ruling4_cut_frames_stay_bili_cut_off_and_warnings_stay_warnings() {
        use crate::history::EventKind;
        for json in [FIX_CUT_OFF, FIX_CUT_OFF_V2] {
            let message = parse(json);
            assert!(records_as_cut_off(&message), "{json}");
            assert!(
                matches!(
                    room_history_event(&message, 1, None).unwrap(),
                    EventKind::BiliCutOff { .. }
                ),
                "{json}"
            );
        }
        for json in [FIX_WARNING_1, FIX_WARNING_2, FIX_MASK] {
            assert!(!records_as_cut_off(&parse(json)), "{json}");
        }
    }

    #[test]
    fn ruling4_full_screen_mask_is_a_warning_with_the_title_only() {
        use crate::history::EventKind;
        let message = parse(FIX_MASK);
        assert!(matches!(
            room_history_event(&message, 1, Some(4)).unwrap(),
            EventKind::BiliWarning { seq: Some(4), room: 1, ref msg, rank: 2, merge: false, stop: true }
                if msg == "当前直播间涉嫌违规"
        ));
        assert_eq!(
            violation_log_line(&message).as_deref(),
            Some("Cut off/Warning: 当前直播间涉嫌违规")
        );
        let (_, msg, _) = history(&message);
        for never in [
            "主播及时更改",
            "example.invalid",
            "http",
            "9999999999",
            "60",
        ] {
            assert!(!msg.contains(never), "{never}: {msg}");
        }
        // The title is cleaned and stripped of links like every reason.
        let dirty = audience_message(
            "FULL_SCREEN_MASK_OPEN",
            serde_json::json!({"title": "涉嫌\n违规\u{202e} https://x.example.invalid/a", "lines": "不读"}),
        );
        assert_eq!(history(&dirty), (false, "涉嫌 违规 [链接已省略]".into(), 2));
        // No title: the existing fallback text, as a warning.
        assert_eq!(
            history(&parse(
                r#"{"cmd":"FULL_SCREEN_MASK_OPEN","data":{"lines":"只有说明"}}"#
            )),
            (false, "直播间涉嫌违规".into(), 0)
        );
        assert_eq!(
            violation_log_line(&parse(r#"{"cmd":"FULL_SCREEN_MASK_OPEN"}"#)).as_deref(),
            Some("Cut off/Warning: Stream cut off")
        );
    }

    #[test]
    fn ruling4_a_mask_still_stops_the_room_like_a_cut() {
        // Classification changed, the stop did not: the mask is captured as
        // a warning event and routed to the same stop arm as CUT_OFF.
        for cmd in [
            "CUT_OFF",
            "CUT_OFF_V2",
            "ANCHOR_ECOLOGY_LIVING_DIALOG",
            "FULL_SCREEN_MASK_OPEN",
        ] {
            assert_eq!(violation_stop(cmd), Some(StopOrder::DanmakuFirst), "{cmd}");
        }
        assert_eq!(violation_stop("WARNING"), Some(StopOrder::StopFirst));
        for cmd in ["ROOM_LOCK", "PREPARING", "DANMU_MSG"] {
            assert_eq!(violation_stop(cmd), None, "{cmd}");
        }
        let warnings = Arc::new(WarningTracker::default());
        assert!(
            capture_warning_message(&warnings, &parse(FIX_MASK)).is_some(),
            "a mask yields the warning event the stop task runs on"
        );
    }

    // ------------------------------------------- Clerk gate L1, item 4

    fn stripped(raw: &str) -> String {
        clean_bili_text(raw)
    }

    #[test]
    fn l1_scheme_links_with_full_width_or_spaced_separators_are_dropped() {
        assert_eq!(stripped("看 https://a.example/x 吧"), "看 [链接已省略] 吧");
        assert_eq!(
            stripped("看 https：／／a.example/x 吧"),
            "看 [链接已省略] 吧"
        );
        assert_eq!(stripped("看https：//a.example/x。"), "看[链接已省略]。");
        assert_eq!(stripped("看 https:// a.example/x 吧"), "看 [链接已省略] 吧");
        assert_eq!(
            stripped("看 https://   a.example/x 吧"),
            "看 [链接已省略] 吧"
        );
        // A non-ASCII path no longer ends the link early.
        assert_eq!(
            stripped("见 https://a.example/路径/秘密 后面"),
            "见 [链接已省略] 后面"
        );
    }

    #[test]
    fn l1_www_and_short_links_without_a_scheme_are_dropped() {
        assert_eq!(
            stripped("访问 www.example.invalid/x 了解"),
            "访问 [链接已省略] 了解"
        );
        assert_eq!(
            stripped("访问WWW.Example.invalid，了解"),
            "访问[链接已省略]，了解"
        );
        assert_eq!(
            stripped("短链 b23.tv/AbCdEf 详情"),
            "短链 [链接已省略] 详情"
        );
        assert_eq!(
            stripped("短链b23.tv/AbCdEf（详情）"),
            "短链[链接已省略]（详情）"
        );
        // Not links: numbers, versions, a dotted word without '/'.
        assert_eq!(
            stripped("版本 1.5/2 与 v2.0 和 a.b"),
            "版本 1.5/2 与 v2.0 和 a.b"
        );
    }

    #[test]
    fn item4_a_dialog_without_a_result_row_is_a_warning() {
        use crate::history::EventKind;
        let message = parse(
            r#"{"cmd":"ANCHOR_ECOLOGY_LIVING_DIALOG","data":{"dialog_title":"直播间违规","dialog_message_list":[
              {"label":"违规原因","content":"您本场直播存在挂机、录播等消极直播行为，请及时整改"},
              {"label":"处罚时间","content":"2024年11月13日21时34分"}]}}"#,
        );
        assert!(!records_as_cut_off(&message));
        assert!(matches!(
            room_history_event(&message, 1, None).unwrap(),
            EventKind::BiliWarning { ref msg, rank: 6, merge: false, .. }
                if msg == "您本场直播存在挂机、录播等消极直播行为，请及时整改"
        ));
        // The stop path is the same as for a cut.
        assert_eq!(
            violation_stop("ANCHOR_ECOLOGY_LIVING_DIALOG"),
            Some(StopOrder::DanmakuFirst)
        );
    }

    #[test]
    fn item4_mask_title_with_a_link_and_control_characters_is_fully_cleaned() {
        use crate::history::EventKind;
        let message = audience_message(
            "FULL_SCREEN_MASK_OPEN",
            serde_json::json!({
                "has_mask": true,
                "icon": "https://i0.example.invalid/mask.png",
                "lines": "主播及时更改，可继续直播，若继续违规，直播间将被关闭。",
                "overlay_second": 60,
                "room_id": 9999999999u64,
                "title": "当前直播间\r\n涉嫌违规\u{0}\u{1b}[31m 详见 www.example.invalid/r 或 b23.tv/x1\u{202e}"
            }),
        );
        let expected = "当前直播间 涉嫌违规 [31m 详见 [链接已省略] 或 [链接已省略]";
        assert!(matches!(
            room_history_event(&message, 1, None).unwrap(),
            EventKind::BiliWarning { ref msg, rank: 2, merge: false, .. } if msg == expected
        ));
        let log = violation_log_line(&message).unwrap();
        assert_eq!(log, format!("Cut off/Warning: {expected}"));
        for never in [
            "主播及时更改",
            "example.invalid",
            "b23.tv",
            "http",
            "\u{1b}",
            "\n",
        ] {
            assert!(!log.contains(never), "{never}: {log:?}");
        }
        assert!(!records_as_cut_off(&message));
        assert_eq!(
            violation_stop("FULL_SCREEN_MASK_OPEN"),
            Some(StopOrder::DanmakuFirst)
        );
    }

    #[test]
    fn l1a_any_double_slash_starts_a_link_and_full_width_separators_are_converted() {
        // The old rule: `://` starts a link even with no ASCII scheme before
        // it (`看://…`, full-width letters).
        assert_eq!(stripped("看://x.example 吧"), "看[链接已省略] 吧");
        assert_eq!(
            stripped("看 ｈｔｔｐｓ://x.example 吧"),
            "看 [链接已省略] 吧"
        );
        assert_eq!(
            stripped("看ｈｔｔｐｓ：／／x.example 吧"),
            "看[链接已省略] 吧"
        );
        // Full-width `：` and `／` are converted character by character, so a
        // mixed separator works too.
        assert_eq!(stripped("看 h：／／x.example 吧"), "看 [链接已省略] 吧");
        assert_eq!(stripped("看 ：／／x.example 吧"), "看 [链接已省略] 吧");
        // A `://` later in a token is still a delimiter, even when the run
        // before it is not a valid scheme (`-https://`, `.://`).
        assert_eq!(stripped(".://x.example 吧"), ".[链接已省略] 吧");
        assert_eq!(stripped("-https://x.example 吧"), "-https[链接已省略] 吧");
    }

    #[test]
    fn l1b_bare_domains_ips_ports_and_full_width_dots_are_dropped() {
        assert_eq!(stripped("短链 b23.tv 详情"), "短链 [链接已省略] 详情");
        assert_eq!(stripped("访问 example.invalid 吧"), "访问 [链接已省略] 吧");
        // An IP address, a host with a port, and an IP with a port.
        assert_eq!(stripped("地址 1.2.3.4 吧"), "地址 [链接已省略] 吧");
        assert_eq!(stripped("地址 1.2.3.4:8080 吧"), "地址 [链接已省略] 吧");
        assert_eq!(
            stripped("端口 example.invalid:8443 吧"),
            "端口 [链接已省略] 吧"
        );
        // Full-width and ideographic dots work as host dots.
        assert_eq!(stripped("短链 b23．tv/x 详情"), "短链 [链接已省略] 详情");
        assert_eq!(stripped("短链 b23。tv 详情"), "短链 [链接已省略] 详情");
        assert_eq!(stripped("短链 b23｡tv 详情"), "短链 [链接已省略] 详情");
        // The ideographic dot stays a host dot inside a scheme URL, but a
        // sentence continues after it when prose follows.
        assert_eq!(stripped("看 https://b23。tv 吧"), "看 [链接已省略] 吧");
        assert_eq!(
            stripped("看 https://x.com。B站官方 吧"),
            "看 [链接已省略]。B站官方 吧"
        );
        // Running Chinese text and punctuation right after a bare host do not
        // keep the host visible.
        assert_eq!(
            stripped("如b23.tv等第三方链接 吧"),
            "如[链接已省略]等第三方链接 吧"
        );
        assert_eq!(stripped("见 b23.tv. 吧"), "见 [链接已省略]. 吧");
        assert_eq!(stripped("短链b23.tv!详情"), "短链[链接已省略]!详情");
        // Still not links: numbers, versions, and a dotted word without a
        // host-shaped last label.
        assert_eq!(
            stripped("版本 1.5/2 与 v2.0 和 a.b"),
            "版本 1.5/2 与 v2.0 和 a.b"
        );
        assert_eq!(stripped("端口 8080 与 2024.11"), "端口 8080 与 2024.11");
    }

    #[test]
    fn the_notice_copy_is_reason_aware_and_exemption_aware() {
        use crate::plugins::danmaku::warning::BroadcastSource;
        use crate::plugins::warning_exempt::{ExemptChannel, ReasonId};

        let tracker = std::sync::Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "星海频道", "UCnotice-fixture", Some("ep-1"));
        let _owner = tracker.own_source(1, source.clone());
        let mut event = tracker.capture_event();
        event.set_reason_ids([ReasonId::IdleRecording].into_iter().collect());
        tracker.mark_event(&mut event, None).unwrap();

        let mut cfg = crate::test_support::config("local", 0);
        assert_eq!(
            violation_notice_for(&cfg, &tracker, &event),
            "🚫挂机判定，暂停转播 星海频道，请换台。"
        );

        // The exemption lets the source restart, so the continues copy goes.
        cfg.warning_hold_exempt =
            vec![ExemptChannel::new("YT", "UCnotice-fixture", "星海频道").unwrap()];
        cfg.warning_hold_exempt_reasons = vec!["idle_recording".into()];
        assert_eq!(
            violation_notice_for(&cfg, &tracker, &event),
            "🚫挂机判定，稍后自动恢复。"
        );
    }

    #[test]
    fn a_code_zero_reply_with_a_message_counts_as_rejected() {
        assert!(danmaku_reply_accepted(&serde_json::json!({"code": 0})));
        assert!(danmaku_reply_accepted(
            &serde_json::json!({"code": 0, "message": ""})
        ));
        assert!(!danmaku_reply_accepted(
            &serde_json::json!({"code": 0, "message": "msg repeat"})
        ));
        assert!(!danmaku_reply_accepted(
            &serde_json::json!({"code": 0, "message": " 过滤 "})
        ));
    }
}
