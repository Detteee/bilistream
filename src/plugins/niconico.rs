use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use lazy_static::lazy_static;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::ffmpeg::PipedIngest;
use super::utils::{command_output_with_timeout, configure_no_window, executable_command};
use crate::config::Niconico as NiconicoConfig;

lazy_static! {
    static ref LIVE_ID_FULL: Regex = Regex::new(r"live\.nicovideo\.jp/watch/(lv\d+)").unwrap();
    static ref LIVE_ID_PATH: Regex = Regex::new(r"/watch/(lv\d+)").unwrap();
    static ref TITLE_ALT: Regex = Regex::new(r#"alt="([^"]+)""#).unwrap();
    static ref TITLE_CLASS: Regex = Regex::new(r#"class="title"[^>]*>\s*<a[^>]*>([^<]+)"#).unwrap();
    static ref HTML_TAGS: Regex = Regex::new(r"<[^>]+>").unwrap();
    static ref START_FULL: Regex = Regex::new(
        r"放送開始：\s*(\d{4})/(\d{1,2})/(\d{1,2})\s*\([^)]*\)\s*(\d{1,2}):(\d{2})(?::(\d{2}))?"
    )
    .unwrap();
    static ref START_KAIEN: Regex =
        Regex::new(r"(\d{1,2})月(\d{1,2})日\s*\([^)]*\)\s*(\d{1,2})時(\d{1,2})分").unwrap();
    static ref EMBEDDED_DATA_PROPS: Regex =
        Regex::new(r#"<script[^>]*id="embedded-data"[^>]*data-props="([^"]+)""#).unwrap();
    static ref EMBEDDED_DATA_PROPS_ALT: Regex =
        Regex::new(r#"<script[^>]*data-props="([^"]+)"[^>]*id="embedded-data""#).unwrap();
    static ref OG_IMAGE: Regex = Regex::new(r#"property="og:image"\s+content="([^"]+)""#).unwrap();
    static ref OG_IMAGE_REV: Regex =
        Regex::new(r#"content="([^"]+)"\s+property="og:image""#).unwrap();
    static ref LISTING_W: Regex = Regex::new(r"([?&]w=)\d+").unwrap();
    static ref LISTING_H: Regex = Regex::new(r"([?&]h=)\d+").unwrap();
    static ref LISTING_THUMB_SRC: Regex =
        Regex::new(r#"src="(https://listing-thumbnail\.live\.nicovideo\.jp[^"]+)""#).unwrap();
}

/// Bilibili `new_room_cover` accepts Twitch 640×360 and YouTube sddefault
/// 640×480. 320×180 is rejected (`42604`); 1280×720 is too large.
const BILI_COVER_WIDTH: &str = "640";
const BILI_COVER_HEIGHT: &str = "360";

const STREAMLINK_TIMEOUT: Duration = Duration::from_secs(45);
const CHANNEL_STATUS_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const CHANNEL_PAGE_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

pub struct Niconico {
    cfg: NiconicoConfig,
}

impl Niconico {
    pub fn new(cfg: &NiconicoConfig) -> Self {
        Niconico { cfg: cfg.clone() }
    }

    pub async fn get_status(
        &self,
    ) -> Result<
        (
            bool,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<chrono::DateTime<chrono::Local>>,
            Option<String>,
        ),
        Box<dyn Error>,
    > {
        get_niconico_status(&self.cfg).await
    }
}

pub fn normalize_live_id(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let path = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let last = path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path);

    if last.starts_with("lv") {
        last.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Channel slug (`vspo`), `ch2648162`, or a `ch.nicovideo.jp/...` URL.
pub fn normalize_channel_id(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains("live.nicovideo.jp/watch/") {
        return String::new();
    }

    let without_host = trimmed
        .strip_prefix("https://ch.nicovideo.jp/")
        .or_else(|| trimmed.strip_prefix("http://ch.nicovideo.jp/"))
        .or_else(|| trimmed.strip_prefix("https://www.nicovideo.jp/"))
        .unwrap_or(trimmed)
        .trim_start_matches('/');

    let first = without_host
        .split(['/', '?', '#'])
        .find(|part| !part.is_empty())
        .unwrap_or("");

    if first.is_empty() || first == "watch" || first.starts_with("lv") || first.contains(':') {
        return String::new();
    }

    first.to_string()
}

pub fn watch_url(live_id: &str) -> String {
    format!(
        "https://live.nicovideo.jp/watch/{}",
        normalize_live_id(live_id)
    )
}

/// The `lv…` id of a `live.nicovideo.jp/watch/` link.
pub fn live_id_from_link(link: &str) -> Option<String> {
    if !link.contains("nicovideo.jp") {
        return None;
    }
    let id = normalize_live_id(link);
    id.starts_with("lv").then_some(id)
}

pub fn channel_live_url(channel_id: &str) -> String {
    format!(
        "https://ch.nicovideo.jp/{}/live",
        normalize_channel_id(channel_id)
    )
}

pub fn niconico_channel_id(cfg: &NiconicoConfig) -> String {
    normalize_channel_id(&cfg.channel_id)
}

pub fn niconico_channel_identity(cfg: &NiconicoConfig) -> (String, String) {
    let id = niconico_channel_id(cfg);
    let name = if id.is_empty() {
        cfg.channel_name.trim().to_string()
    } else {
        niconico_name_from_channels_file(&id).unwrap_or_else(|| cfg.channel_name.trim().to_string())
    };
    (id, name)
}

pub fn niconico_channel_name(cfg: &NiconicoConfig) -> String {
    niconico_channel_identity(cfg).1
}

fn niconico_restream_name_from_channel(channel: &crate::config::Channel) -> String {
    channel
        .niconico_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(channel.name.trim())
        .to_string()
}

fn niconico_name_from_channels(
    channels: &[crate::config::Channel],
    channel_id: &str,
) -> Option<String> {
    let id = normalize_channel_id(channel_id);
    if id.is_empty() {
        return None;
    }
    channels.iter().find_map(|channel| {
        let nico_id = channel.platforms.niconico.as_deref()?;
        (normalize_channel_id(nico_id) == id).then(|| niconico_restream_name_from_channel(channel))
    })
}

fn niconico_name_from_channels_file(channel_id: &str) -> Option<String> {
    crate::config::load_channels()
        .ok()
        .and_then(|channels| niconico_name_from_channels(&channels.channels, channel_id))
}

pub fn niconico_configured(cfg: &NiconicoConfig) -> bool {
    !niconico_channel_id(cfg).is_empty() || !normalize_live_id(&cfg.live_id).is_empty()
}

pub fn streamlink_ingest(
    cfg: &NiconicoConfig,
    live_id: &str,
) -> Result<PipedIngest, Box<dyn Error>> {
    let live_id = normalize_live_id(live_id);
    if live_id.is_empty() {
        return Err("Niconico live ID is empty".into());
    }

    let quality = if cfg.quality.trim().is_empty() {
        "best"
    } else {
        cfg.quality.trim()
    };

    let mut args = streamlink_auth_args(cfg)?;
    args.push("--ffmpeg-copyts".to_string());
    args.push("--stdout".to_string());
    args.push(watch_url(&live_id));
    args.push(quality.to_string());

    Ok(PipedIngest {
        program: streamlink_command(),
        args,
    })
}

fn streamlink_command() -> String {
    executable_command("streamlink.exe", "streamlink")
}

fn streamlink_auth_args(cfg: &NiconicoConfig) -> Result<Vec<String>, Box<dyn Error>> {
    let session = user_session_from_cookies_file(&resolve_cookies_path(cfg)?)?;
    let mut args = Vec::new();
    if let Some(proxy) = cfg.proxy.as_deref().filter(|proxy| !proxy.is_empty()) {
        args.push("--http-proxy".to_string());
        args.push(proxy.to_string());
    }
    args.push("--niconico-user-session".to_string());
    args.push(session);
    Ok(args)
}

fn resolve_cookies_path(cfg: &NiconicoConfig) -> Result<PathBuf, Box<dyn Error>> {
    let Some(path) = cfg.cookies_file.as_deref().filter(|path| !path.is_empty()) else {
        return Err("Niconico cookies file is not configured".into());
    };

    let given = PathBuf::from(path);
    if given.exists() {
        return Ok(given);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(path);
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }
    Err(format!("Niconico cookies file not found: {}", path).into())
}

pub(crate) fn user_session_from_cookies_file(path: &Path) -> Result<String, Box<dyn Error>> {
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        let Some(record) = netscape_cookie_record(line) else {
            continue;
        };
        let parts: Vec<&str> = record.split('\t').collect();
        if parts.len() >= 7 && parts[5] == "user_session" && !parts[6].is_empty() {
            return Ok(parts[6].to_string());
        }
    }
    Err("niconico cookies file is missing user_session".into())
}

/// Cookie-Editor / curl mark HttpOnly cookies with a `#HttpOnly_` prefix on
/// the domain. Those are still records, not comments.
fn netscape_cookie_record(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(record) = trimmed.strip_prefix("#HttpOnly_") {
        return (!record.is_empty()).then_some(record);
    }
    (!trimmed.starts_with('#')).then_some(trimmed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelProgram {
    pub(crate) live_id: String,
    pub(crate) title: Option<String>,
    pub(crate) start_at: Option<DateTime<Local>>,
    pub(crate) thumbnail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelLiveListing {
    OnAir(ChannelProgram),
    Scheduled(ChannelProgram),
    Idle,
}

fn niconico_jst() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).expect("JST offset")
}

fn html_class_section<'a>(html: &'a str, class: &str) -> Option<&'a str> {
    let double = format!("class=\"sub {class}\"");
    let single = format!("class='sub {class}'");
    let start = html.find(&double).or_else(|| html.find(&single))?;
    let after = html[start..].find('>')? + start + 1;
    let end = html[after..].find("</section>")?;
    Some(&html[after..after + end])
}

fn html_id_block<'a>(html: &'a str, id: &str, terminators: &[&str]) -> Option<&'a str> {
    let double = format!("id=\"{id}\"");
    let single = format!("id='{id}'");
    let start = html.find(&double).or_else(|| html.find(&single))?;
    let rest = &html[start..];
    let end = terminators
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len().min(12000));
    Some(&rest[..end])
}

fn now_section(html: &str) -> Option<&str> {
    html_class_section(html, "now").or_else(|| {
        html_id_block(
            html,
            "live_now",
            &[
                r#"id="live_future""#,
                "id='live_future'",
                r#"id="live_past""#,
                "id='live_past'",
                ">放送予定<",
                ">過去<",
            ],
        )
    })
}

fn future_section(html: &str) -> Option<&str> {
    html_class_section(html, "future").or_else(|| {
        html_id_block(
            html,
            "live_future",
            &[
                r#"id="live_past""#,
                "id='live_past'",
                ">過去の放送<",
                ">過去<",
            ],
        )
    })
}

fn parse_channel_program(section: &str) -> Option<ChannelProgram> {
    if section.contains("次回予定はまだ登録されていません") {
        return None;
    }
    let live_id =
        regex_first(&LIVE_ID_FULL, section).or_else(|| regex_first(&LIVE_ID_PATH, section))?;
    let title = regex_first(&TITLE_ALT, section)
        .or_else(|| regex_first(&TITLE_CLASS, section))
        .map(|title| html_unescape(&title));
    let start_at = parse_program_start(section);
    Some(ChannelProgram {
        live_id,
        title,
        start_at,
        thumbnail: parse_program_thumbnail(section),
    })
}

fn parse_channel_live_listing(html: &str) -> ChannelLiveListing {
    if let Some(program) = now_section(html).and_then(parse_channel_program) {
        return ChannelLiveListing::OnAir(program);
    }
    if let Some(program) = future_section(html).and_then(parse_channel_program) {
        return ChannelLiveListing::Scheduled(program);
    }
    ChannelLiveListing::Idle
}

fn parse_program_start(section: &str) -> Option<DateTime<Local>> {
    parse_program_start_at(section, Utc::now().with_timezone(&niconico_jst()))
}

fn parse_program_thumbnail(section: &str) -> Option<String> {
    regex_first(&LISTING_THUMB_SRC, section).and_then(|url| bili_ready_listing_url(&url))
}

fn parse_program_start_at(
    section: &str,
    now_jst: DateTime<FixedOffset>,
) -> Option<DateTime<Local>> {
    let text = HTML_TAGS.replace_all(section, "");
    let text = html_unescape(&text);
    if let Some(caps) = START_FULL.captures(&text) {
        let year: i32 = caps.get(1)?.as_str().parse().ok()?;
        let month: u32 = caps.get(2)?.as_str().parse().ok()?;
        let day: u32 = caps.get(3)?.as_str().parse().ok()?;
        let hour: u32 = caps.get(4)?.as_str().parse().ok()?;
        let minute: u32 = caps.get(5)?.as_str().parse().ok()?;
        let second: u32 = caps
            .get(6)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        return jst_local(year, month, day, hour, minute, second);
    }
    let caps = START_KAIEN.captures(&text)?;
    let month: u32 = caps.get(1)?.as_str().parse().ok()?;
    let day: u32 = caps.get(2)?.as_str().parse().ok()?;
    let hour: u32 = caps.get(3)?.as_str().parse().ok()?;
    let minute: u32 = caps.get(4)?.as_str().parse().ok()?;
    let mut year = now_jst.year();
    let mut dt = jst_local(year, month, day, hour, minute, 0)?;
    let dt_jst = dt.with_timezone(&niconico_jst());
    if dt_jst + chrono::Duration::hours(12) < now_jst {
        year += 1;
        dt = jst_local(year, month, day, hour, minute, 0)?;
    }
    Some(dt)
}

fn jst_local(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<DateTime<Local>> {
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_opt(hour, minute, second)?;
    niconico_jst()
        .from_local_datetime(&date.and_time(time))
        .single()
        .map(|dt| dt.with_timezone(&Local))
}

fn regex_first(pattern: &Regex, haystack: &str) -> Option<String> {
    pattern
        .captures(haystack)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|value| !value.is_empty())
}

fn html_unescape(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn niconico_page_request(
    url: &str,
    proxy: Option<&str>,
) -> Result<reqwest::RequestBuilder, Box<dyn Error>> {
    Ok(super::http::pooled_client(proxy)?
        .get(url)
        .timeout(CHANNEL_STATUS_TIMEOUT)
        .header(reqwest::header::USER_AGENT, CHANNEL_PAGE_USER_AGENT)
        .version(reqwest::Version::HTTP_11))
}

pub(crate) async fn fetch_channel_listing(
    channel_id: &str,
    proxy: Option<&str>,
) -> Result<ChannelLiveListing, Box<dyn Error>> {
    let url = channel_live_url(channel_id);
    let request = niconico_page_request(&url, proxy)?;
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(format!("Niconico channel page HTTP {}", response.status()).into());
    }
    let bytes = super::http::response_bytes_limited(response, 8 * 1024 * 1024).await?;
    let html = String::from_utf8_lossy(&bytes);
    Ok(parse_channel_live_listing(&html))
}

#[derive(Debug, Deserialize)]
struct EmbeddedWatchData {
    #[serde(default)]
    program: Option<EmbeddedProgram>,
}

#[derive(Debug, Deserialize)]
struct EmbeddedProgram {
    #[serde(default)]
    thumbnail: Option<EmbeddedThumbnail>,
}

#[derive(Debug, Deserialize)]
struct EmbeddedThumbnail {
    #[serde(default)]
    huge: Option<EmbeddedHugeThumbnail>,
}

#[derive(Debug, Deserialize)]
struct EmbeddedHugeThumbnail {
    #[serde(default, rename = "s1920x1080")]
    s1920x1080: Option<String>,
    #[serde(default, rename = "s1280x720")]
    s1280x720: Option<String>,
    #[serde(default, rename = "s640x360")]
    s640x360: Option<String>,
}

fn bili_ready_listing_url(url: &str) -> Option<String> {
    let unescaped = html_unescape(url);
    if !(unescaped.starts_with("https://")
        && unescaped.contains("listing-thumbnail.live.nicovideo.jp"))
    {
        return None;
    }

    let mut sized = if LISTING_W.is_match(&unescaped) {
        LISTING_W
            .replace(&unescaped, format!("${{1}}{BILI_COVER_WIDTH}").as_str())
            .into_owned()
    } else if unescaped.contains('?') {
        format!("{unescaped}&w={BILI_COVER_WIDTH}")
    } else {
        format!("{unescaped}?w={BILI_COVER_WIDTH}")
    };
    if LISTING_H.is_match(&sized) {
        sized = LISTING_H
            .replace(&sized, format!("${{1}}{BILI_COVER_HEIGHT}").as_str())
            .into_owned();
    } else {
        sized = format!("{sized}&h={BILI_COVER_HEIGHT}");
    }
    Some(sized)
}

/// Program listing thumbnail for Bilibili cover, forced to 640×360 (same as
/// Twitch). Channel icons (64×64 / 128×128) are skipped.
pub(crate) fn parse_niconico_cover_thumbnail_url(html: &str) -> Option<String> {
    thumbnail_from_embedded_data(html).or_else(|| thumbnail_from_og_image(html))
}

fn thumbnail_from_embedded_data(html: &str) -> Option<String> {
    let props = regex_first(&EMBEDDED_DATA_PROPS, html)
        .or_else(|| regex_first(&EMBEDDED_DATA_PROPS_ALT, html))?;
    let data: EmbeddedWatchData = serde_json::from_str(&html_unescape(&props)).ok()?;
    let huge = data.program?.thumbnail?.huge?;
    [huge.s640x360, huge.s1280x720, huge.s1920x1080]
        .into_iter()
        .find_map(|url| url.as_deref().and_then(bili_ready_listing_url))
}

fn thumbnail_from_og_image(html: &str) -> Option<String> {
    regex_first(&OG_IMAGE, html)
        .or_else(|| regex_first(&OG_IMAGE_REV, html))
        .and_then(|url| bili_ready_listing_url(&url))
}

pub async fn niconico_cover_thumbnail_url(
    live_id: &str,
    proxy: Option<&str>,
) -> Result<Option<String>, Box<dyn Error>> {
    let live_id = normalize_live_id(live_id);
    if live_id.is_empty() {
        return Ok(None);
    }
    let request = niconico_page_request(&watch_url(&live_id), proxy)?;
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(format!("Niconico watch page HTTP {}", response.status()).into());
    }
    let bytes = super::http::response_bytes_limited(response, 8 * 1024 * 1024).await?;
    let html = String::from_utf8_lossy(&bytes);
    Ok(parse_niconico_cover_thumbnail_url(&html))
}

#[derive(Debug, Deserialize)]
struct StreamlinkJson {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    metadata: Option<StreamlinkMetadata>,
    #[serde(default)]
    streams: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct StreamlinkMetadata {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

pub async fn get_niconico_status(
    cfg: &NiconicoConfig,
) -> Result<
    (
        bool,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Local>>,
        Option<String>,
    ),
    Box<dyn Error>,
> {
    let channel_id = niconico_channel_id(cfg);
    if !channel_id.is_empty() {
        return match fetch_channel_listing(&channel_id, cfg.proxy.as_deref()).await {
            Ok(ChannelLiveListing::OnAir(program)) => Ok((
                true,
                None,
                program.title,
                Some(watch_url(&program.live_id)),
                None,
                Some(program.live_id),
            )),
            Ok(ChannelLiveListing::Scheduled(program)) => Ok((
                false,
                None,
                program.title,
                Some(watch_url(&program.live_id)),
                program.start_at,
                Some(program.live_id),
            )),
            Ok(ChannelLiveListing::Idle) => Ok((false, None, None, None, None, None)),
            Err(e) => {
                tracing::warn!("Niconico 频道直播页查询失败: {}", e);
                Err(e)
            }
        };
    }

    let live_id = normalize_live_id(&cfg.live_id);
    if live_id.is_empty() {
        return Ok((false, None, None, None, None, None));
    }
    get_niconico_status_via_streamlink(&live_id, cfg).await
}

async fn get_niconico_status_via_streamlink(
    live_id: &str,
    cfg: &NiconicoConfig,
) -> Result<
    (
        bool,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Local>>,
        Option<String>,
    ),
    Box<dyn Error>,
> {
    let mut args = streamlink_auth_args(cfg)?;
    args.push("--json".to_string());
    args.push(watch_url(live_id));

    let mut command = Command::new(streamlink_command());
    configure_no_window(&mut command);
    command.args(&args);

    let output = match command_output_with_timeout(command, STREAMLINK_TIMEOUT, "streamlink").await
    {
        Ok(output) => output,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err("streamlink 未安装或不在 PATH 中。".into());
            }
            return Err(e.into());
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = match serde_json::from_str::<StreamlinkJson>(stdout.trim()) {
        Ok(parsed) => parsed,
        Err(_) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No playable streams") || stdout.contains("No playable streams") {
                return Ok((false, None, None, None, None, Some(live_id.to_string())));
            }
            return Err(format!("streamlink niconico status failed: {}", stderr.trim()).into());
        }
    };

    if let Some(error) = parsed.error.as_deref().filter(|error| !error.is_empty()) {
        if error.contains("No playable streams") {
            return Ok((false, None, None, None, None, Some(live_id.to_string())));
        }
        return Err(error.to_string().into());
    }

    if parsed.streams.is_empty() {
        return Ok((false, None, None, None, None, Some(live_id.to_string())));
    }

    let metadata = parsed.metadata.unwrap_or_default();
    let title = metadata
        .title
        .filter(|title| !title.is_empty())
        .or(metadata.author.filter(|author| !author.is_empty()));
    let stream_id = metadata
        .id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| live_id.to_string());

    Ok((
        true,
        None,
        title,
        Some(watch_url(live_id)),
        None,
        Some(stream_id),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use std::io::Write;

    #[test]
    fn normalize_live_id_strips_watch_url() {
        assert_eq!(
            normalize_live_id("https://live.nicovideo.jp/watch/lv351182284"),
            "lv351182284"
        );
        assert_eq!(
            normalize_live_id("https://live.nicovideo.jp/watch/lv351182284?ref=1"),
            "lv351182284"
        );
        assert_eq!(normalize_live_id(" lv123 "), "lv123");
        assert_eq!(normalize_live_id(""), "");
    }

    #[test]
    fn live_id_from_link_needs_a_nicovideo_watch_url() {
        assert_eq!(
            live_id_from_link("https://live.nicovideo.jp/watch/lv351182284?ref=1").as_deref(),
            Some("lv351182284")
        );
        assert_eq!(live_id_from_link("https://www.twitch.tv/vspo"), None);
        assert_eq!(live_id_from_link("https://ch.nicovideo.jp/vspo/live"), None);
    }

    #[test]
    fn normalize_channel_id_from_slug_and_url() {
        assert_eq!(normalize_channel_id("vspo"), "vspo");
        assert_eq!(normalize_channel_id("https://ch.nicovideo.jp/vspo"), "vspo");
        assert_eq!(
            normalize_channel_id("https://ch.nicovideo.jp/vspo/live"),
            "vspo"
        );
        assert_eq!(normalize_channel_id("ch2648162"), "ch2648162");
        assert_eq!(
            normalize_channel_id("https://live.nicovideo.jp/watch/lv351182284"),
            ""
        );
        assert_eq!(normalize_channel_id(""), "");
    }

    #[test]
    fn pinned_live_id_is_configured_without_channel_id() {
        let cfg = crate::config::Niconico {
            live_id: "lv351182284".to_string(),
            ..Default::default()
        };
        assert_eq!(niconico_channel_id(&cfg), "");
        assert!(niconico_configured(&cfg));
    }

    #[test]
    fn niconico_restream_name_prefers_niconico_name_over_youtube_name() {
        let channel = crate::config::Channel {
            name: "ぶいすぽっ!【公式】".to_string(),
            niconico_name: Some("ぶいすぽ激ロー".to_string()),
            aliases: vec!["vspo".to_string()],
            platforms: crate::config::ChannelPlatforms {
                youtube: Some("UCuI5XaO-6VkOEhHao6ij7JA".to_string()),
                twitch: None,
                niconico: Some("vspo".to_string()),
            },
            riot_puuid: None,
        };
        assert_eq!(
            niconico_restream_name_from_channel(&channel),
            "ぶいすぽ激ロー"
        );
        assert_eq!(
            niconico_name_from_channels(std::slice::from_ref(&channel), "vspo").as_deref(),
            Some("ぶいすぽ激ロー")
        );
        assert_eq!(
            niconico_name_from_channels(
                std::slice::from_ref(&channel),
                "https://ch.nicovideo.jp/vspo"
            )
            .as_deref(),
            Some("ぶいすぽ激ロー")
        );

        let youtube_only = crate::config::Channel {
            name: "ぶいすぽっ!【公式】".to_string(),
            niconico_name: None,
            aliases: vec![],
            platforms: crate::config::ChannelPlatforms {
                youtube: Some("UCuI5XaO-6VkOEhHao6ij7JA".to_string()),
                twitch: None,
                niconico: Some("vspo".to_string()),
            },
            riot_puuid: None,
        };
        assert_eq!(
            niconico_restream_name_from_channel(&youtube_only),
            "ぶいすぽっ!【公式】"
        );
    }

    #[test]
    fn parse_channel_onair_reads_live_now_block() {
        let html = r#"
<div id="live_now">
  <h1>放送中</h1>
  <div id="live_now_cnt">
    <ul class="items">
      <li class="item">
        <a href="https://live.nicovideo.jp/watch/lv351182284" class="thumb_live">
          <img alt="【#ぶいすぽ激ロー】ノンデリ王2026">
        </a>
        <p class="title"><a href="https://live.nicovideo.jp/watch/lv351182284">【#ぶいすぽ激ロー】ノンデリ王2026</a></p>
      </li>
    </ul>
  </div>
</div>
<div id="live_future"><h1>放送予定</h1></div>
"#;
        match parse_channel_live_listing(html) {
            ChannelLiveListing::OnAir(onair) => {
                assert_eq!(onair.live_id, "lv351182284");
                assert_eq!(
                    onair.title.as_deref(),
                    Some("【#ぶいすぽ激ロー】ノンデリ王2026")
                );
            }
            other => panic!("expected on-air, got {other:?}"),
        }
    }

    #[test]
    fn parse_channel_empty_live_now_is_not_live() {
        let html = r#"
<div id="live_now"><h1>放送中</h1><div id="live_now_cnt"><ul class="items"></ul></div></div>
<div id="live_future"><h1>放送予定</h1>
  <a href="https://live.nicovideo.jp/watch/lv111">upcoming</a>
</div>
"#;
        match parse_channel_live_listing(html) {
            ChannelLiveListing::Scheduled(program) => {
                assert_eq!(program.live_id, "lv111");
            }
            other => panic!("expected scheduled, got {other:?}"),
        }
    }

    #[test]
    fn parse_channel_now_section_class_when_ids_are_absent() {
        let html = r#"
<section class="sub now">
  <div class="item cfix">
    <ul class="items">
      <li class="item">
        <a href="https://live.nicovideo.jp/watch/lv351182284" class="thumb_live">
          <img alt="ライブ中">
        </a>
        <h2 class="title"><a href="https://live.nicovideo.jp/watch/lv351182284">ライブ中</a></h2>
      </li>
    </ul>
  </div>
</section>
<section class="sub future"><h1>放送予定</h1><p class="not_found">次回予定はまだ登録されていません。</p></section>
"#;
        match parse_channel_live_listing(html) {
            ChannelLiveListing::OnAir(program) => {
                assert_eq!(program.live_id, "lv351182284");
                assert_eq!(program.title.as_deref(), Some("ライブ中"));
            }
            other => panic!("expected on-air, got {other:?}"),
        }
    }

    #[test]
    fn parse_channel_future_section_reads_kaien_schedule() {
        let html = r#"
<section class="sub now"></section>
<section class="sub future">
  <h1>放送予定</h1>
  <div class="item cfix">
    <ul class="items">
      <li class="item">
        <a href="https://live.nicovideo.jp/watch/lv351230205" class="thumb_live">
          <img alt="にじさんじのTOYBOX！">
        </a>
        <h2 class="title"><a href="https://live.nicovideo.jp/watch/lv351230205">にじさんじのTOYBOX！</a></h2>
        <p class="date">開演：<strong class="fs14">09月10日 (木) 20時00分</strong></p>
      </li>
    </ul>
  </div>
</section>
"#;
        match parse_channel_live_listing(html) {
            ChannelLiveListing::Scheduled(program) => {
                assert_eq!(program.live_id, "lv351230205");
                assert_eq!(program.title.as_deref(), Some("にじさんじのTOYBOX！"));
                let start = program.start_at.expect("kaien start");
                let jst = start.with_timezone(&niconico_jst());
                assert_eq!(jst.month(), 9);
                assert_eq!(jst.day(), 10);
                assert_eq!(jst.hour(), 20);
                assert_eq!(jst.minute(), 0);
            }
            other => panic!("expected scheduled, got {other:?}"),
        }
    }

    #[test]
    fn parse_channel_future_not_found_is_idle() {
        let html = r#"
<section class="sub now"></section>
<section class="sub future">
  <h1>放送予定</h1>
  <p class="not_found">次回予定はまだ登録されていません。</p>
</section>
<section class="sub past">
  <a href="https://live.nicovideo.jp/watch/lv351182284">past</a>
</section>
"#;
        assert_eq!(parse_channel_live_listing(html), ChannelLiveListing::Idle);
    }

    #[test]
    fn parse_channel_program_reads_the_listing_thumbnail() {
        let html = r#"
<section class="sub future">
  <li class="item">
    <a href="https://live.nicovideo.jp/watch/lv351462902" class="thumb_live">
      <img src="https://listing-thumbnail.live.nicovideo.jp?image=prod-lv351462902/thumbnail_1790327497200.jpg&amp;w=352&amp;h=198&amp;v=1790327497200" alt="クイズ激ロー">
    </a>
    <h2 class="title"><a href="https://live.nicovideo.jp/watch/lv351462902">クイズ激ロー</a></h2>
    <p class="date">開演：<strong class="fs14">09月28日 (月) 19時50分</strong></p>
  </li>
</section>
"#;
        match parse_channel_live_listing(html) {
            ChannelLiveListing::Scheduled(program) => {
                assert_eq!(program.live_id, "lv351462902");
                assert_eq!(
                    program.thumbnail.as_deref(),
                    Some(
                        "https://listing-thumbnail.live.nicovideo.jp?image=prod-lv351462902/thumbnail_1790327497200.jpg&w=640&h=360&v=1790327497200"
                    )
                );
            }
            other => panic!("expected scheduled, got {other:?}"),
        }
    }

    #[test]
    fn kaien_without_year_rolls_forward_when_date_already_passed() {
        let jst = niconico_jst();
        let now = jst.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).single().unwrap();
        let dt = parse_program_start_at("開演：09月10日 (木) 20時00分", now).unwrap();
        assert_eq!(dt.with_timezone(&jst).year(), 2027);
    }

    #[test]
    fn broadcast_start_parses_full_timestamp() {
        let dt = parse_program_start("放送開始：2026/08/29 (土) 19:50:00").unwrap();
        let jst = dt.with_timezone(&niconico_jst());
        assert_eq!(jst.year(), 2026);
        assert_eq!(jst.month(), 8);
        assert_eq!(jst.day(), 29);
        assert_eq!(jst.hour(), 19);
        assert_eq!(jst.minute(), 50);
    }

    #[test]
    fn user_session_reads_netscape_cookie_file() {
        let dir = std::env::temp_dir().join(format!(
            "bilistream-nico-cookie-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "# Netscape HTTP Cookie File\n.nicovideo.jp\tTRUE\t/\tTRUE\t0\tuser_session\tsession-value"
        )
        .unwrap();

        let session = user_session_from_cookies_file(&path).unwrap();
        assert_eq!(session, "session-value");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_session_missing_cookie_returns_error() {
        let dir = std::env::temp_dir().join(format!(
            "bilistream-nico-cookie-missing-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.txt");
        std::fs::write(&path, "# empty\n").unwrap();

        assert!(user_session_from_cookies_file(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_session_reads_httponly_netscape_line() {
        let dir = std::env::temp_dir().join(format!(
            "bilistream-nico-cookie-httponly-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.txt");
        std::fs::write(
            &path,
            "# Netscape HTTP Cookie File\n#HttpOnly_.nicovideo.jp\tTRUE\t/\tTRUE\t0\tuser_session\thttponly-session\n.nicovideo.jp\tTRUE\t/\tTRUE\t0\tnicosid\tsid\n",
        )
        .unwrap();

        let session = user_session_from_cookies_file(&path).unwrap();
        assert_eq!(session, "httponly-session");

        std::fs::remove_dir_all(dir).unwrap();
    }

    fn escaped_embedded_data(huge: &str) -> String {
        format!(
            r#"<script id="embedded-data" data-props="{{&quot;program&quot;:{{&quot;thumbnail&quot;:{{&quot;small&quot;:&quot;https://secure-dcdn.cdn.nimg.jp/comch/channel-icon/64x64/ch1.jpg&quot;,&quot;large&quot;:&quot;https://secure-dcdn.cdn.nimg.jp/comch/channel-icon/128x128/ch1.jpg&quot;,&quot;huge&quot;:{huge}}}}}}}"></script>"#
        )
    }

    #[test]
    fn cover_thumbnail_uses_640x360_even_when_1920_is_listed() {
        let html = escaped_embedded_data(
            r#"{&quot;s1920x1080&quot;:&quot;https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&amp;w=1920&amp;h=1080&quot;,&quot;s1280x720&quot;:&quot;https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&amp;w=1280&amp;h=720&quot;,&quot;s640x360&quot;:&quot;https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&amp;w=640&amp;h=360&quot;}"#,
        );
        assert_eq!(
            parse_niconico_cover_thumbnail_url(&html).as_deref(),
            Some("https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&w=640&h=360")
        );
    }

    #[test]
    fn cover_thumbnail_rewrites_1280_and_1920_listing_urls_to_640x360() {
        let html = escaped_embedded_data(
            r#"{&quot;s1280x720&quot;:&quot;https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&amp;w=1280&amp;h=720&quot;}"#,
        );
        assert_eq!(
            parse_niconico_cover_thumbnail_url(&html).as_deref(),
            Some("https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&w=640&h=360")
        );

        let html = escaped_embedded_data(
            r#"{&quot;s1920x1080&quot;:&quot;https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&amp;w=1920&amp;h=1080&quot;}"#,
        );
        assert_eq!(
            parse_niconico_cover_thumbnail_url(&html).as_deref(),
            Some("https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&w=640&h=360")
        );
    }

    #[test]
    fn cover_thumbnail_skips_channel_icons() {
        let html = escaped_embedded_data("{}");
        assert_eq!(parse_niconico_cover_thumbnail_url(&html), None);
    }

    #[test]
    fn cover_thumbnail_uses_og_image_when_embedded_data_missing() {
        let html = r#"<meta property="og:image" content="https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&amp;w=1280&amp;h=720"/>"#;
        assert_eq!(
            parse_niconico_cover_thumbnail_url(html).as_deref(),
            Some("https://listing-thumbnail.live.nicovideo.jp?image=prod-lv1/t.jpg&w=640&h=360")
        );
    }
}
