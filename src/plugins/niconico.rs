use serde::Deserialize;
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::ffmpeg::PipedIngest;
use super::utils::{command_output_with_timeout, configure_no_window, executable_command};
use crate::config::{load_config, Niconico as NiconicoConfig};

const STREAMLINK_TIMEOUT: Duration = Duration::from_secs(45);
const CHANNEL_STATUS_TIMEOUT: Duration = Duration::from_secs(20);
const CHANNEL_PAGE_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

pub struct Niconico {
    pub channel_id: String,
}

impl Niconico {
    pub fn new(channel_id: &str) -> Self {
        Niconico {
            channel_id: normalize_channel_id(channel_id),
        }
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
        let cfg = load_config().await?;
        get_niconico_status(&cfg.niconico).await
    }
}

pub fn normalize_live_id(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let last = trimmed
        .rsplit(|c| c == '/' || c == '?')
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed);

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

pub fn channel_live_url(channel_id: &str) -> String {
    format!(
        "https://ch.nicovideo.jp/{}/live",
        normalize_channel_id(channel_id)
    )
}

pub fn niconico_channel_id(cfg: &NiconicoConfig) -> String {
    normalize_channel_id(&cfg.channel_id)
}

pub fn niconico_restream_name_from_channel(channel: &crate::config::Channel) -> String {
    channel
        .niconico_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(channel.name.trim())
        .to_string()
}

pub fn niconico_name_from_channels(
    channels: &[crate::config::Channel],
    channel_id: &str,
) -> Option<String> {
    let id = normalize_channel_id(channel_id);
    if id.is_empty() {
        return None;
    }
    channels.iter().find_map(|channel| {
        let nico_id = channel.platforms.niconico.as_deref()?;
        if normalize_channel_id(nico_id) == id {
            Some(niconico_restream_name_from_channel(channel))
        } else {
            None
        }
    })
}

pub fn niconico_channel_name(cfg: &NiconicoConfig) -> String {
    let id = niconico_channel_id(cfg);
    if let Ok(channels) = crate::config::load_channels() {
        if let Some(name) = niconico_name_from_channels(&channels.channels, &id) {
            return name;
        }
    }
    cfg.channel_name.trim().to_string()
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
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() >= 7 && parts[5] == "user_session" && !parts[6].is_empty() {
            return Ok(parts[6].to_string());
        }
    }
    Err("niconico cookies file is missing user_session".into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OnairProgram {
    live_id: String,
    title: Option<String>,
}

fn parse_channel_onair(html: &str) -> Option<OnairProgram> {
    let start = html
        .find(r#"id="live_now""#)
        .or_else(|| html.find("id='live_now'"))?;
    let rest = &html[start..];
    let end = [
        r#"id="live_future""#,
        "id='live_future'",
        r#"id="live_past""#,
        "id='live_past'",
        ">放送予定<",
        ">過去<",
    ]
    .iter()
    .filter_map(|marker| rest.find(marker))
    .min()
    .unwrap_or(rest.len().min(8000));
    let section = &rest[..end];

    let live_id = regex_first(r"live\.nicovideo\.jp/watch/(lv\d+)", section)
        .or_else(|| regex_first(r"/watch/(lv\d+)", section))?;
    let title = regex_first(r#"alt="([^"]+)""#, section)
        .or_else(|| regex_first(r#"class="title"[^>]*>\s*<a[^>]*>([^<]+)"#, section))
        .map(|title| html_unescape(&title));

    Some(OnairProgram { live_id, title })
}

fn regex_first(pattern: &str, haystack: &str) -> Option<String> {
    regex::Regex::new(pattern)
        .ok()?
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

async fn fetch_channel_onair(
    channel_id: &str,
    proxy: Option<&str>,
) -> Result<Option<OnairProgram>, Box<dyn Error>> {
    let url = channel_live_url(channel_id);
    let mut builder = reqwest::Client::builder()
        .timeout(CHANNEL_STATUS_TIMEOUT)
        .user_agent(CHANNEL_PAGE_USER_AGENT)
        .http1_only();
    if let Some(proxy_url) = proxy.filter(|proxy| !proxy.is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
    }
    let client = builder.build()?;
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(format!("Niconico channel page HTTP {}", response.status()).into());
    }
    let html = response.text().await?;
    Ok(parse_channel_onair(&html))
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
        return match fetch_channel_onair(&channel_id, cfg.proxy.as_deref()).await {
            Ok(Some(onair)) => Ok((
                true,
                None,
                onair.title,
                Some(watch_url(&onair.live_id)),
                None,
                Some(onair.live_id),
            )),
            Ok(None) => Ok((false, None, None, None, None, None)),
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
    get_niconico_status_via_streamlink(&live_id, cfg)
}

fn get_niconico_status_via_streamlink(
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

    let output = match command_output_with_timeout(&mut command, STREAMLINK_TIMEOUT, "streamlink") {
        Ok(output) => output,
        Err(e) => {
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return Err("streamlink 未安装或不在 PATH 中。".into());
            }
            return Err(e);
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

    let has_streams = parsed
        .streams
        .keys()
        .any(|name| name != "best" || parsed.streams.len() > 1)
        || parsed.streams.contains_key("best");
    if !has_streams {
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
    use std::io::Write;

    #[test]
    fn normalize_live_id_strips_watch_url() {
        assert_eq!(
            normalize_live_id("https://live.nicovideo.jp/watch/lv351182284"),
            "lv351182284"
        );
        assert_eq!(normalize_live_id(" lv123 "), "lv123");
        assert_eq!(normalize_live_id(""), "");
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
    fn empty_niconico_config_is_not_configured() {
        let cfg = crate::config::Niconico::default();
        assert_eq!(niconico_channel_id(&cfg), "");
        assert_eq!(niconico_channel_name(&cfg), "");
        assert!(!niconico_configured(&cfg));
    }

    #[test]
    fn pinned_live_id_is_configured_without_channel_id() {
        let mut cfg = crate::config::Niconico::default();
        cfg.live_id = "lv351182284".to_string();
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
        let onair = parse_channel_onair(html).expect("onair program");
        assert_eq!(onair.live_id, "lv351182284");
        assert_eq!(
            onair.title.as_deref(),
            Some("【#ぶいすぽ激ロー】ノンデリ王2026")
        );
    }

    #[test]
    fn parse_channel_onair_empty_live_now_is_offline() {
        let html = r#"
<div id="live_now"><h1>放送中</h1><div id="live_now_cnt"><ul class="items"></ul></div></div>
<div id="live_future"><h1>放送予定</h1>
  <a href="https://live.nicovideo.jp/watch/lv111">upcoming</a>
</div>
"#;
        assert!(parse_channel_onair(html).is_none());
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
}
