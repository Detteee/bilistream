//! Notifier settings (design §9).
//!
//! Non-secret preferences live in the local store record `notify-config`
//! with their own revision, so saving them never conflicts with
//! `/api/config` edits. Like `notify-secrets`, the record is not in
//! `storage::NAMES`: it is never synced and not part of backups.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const QQ_DEFAULT_API_BASE: &str = "https://api.sgroup.qq.com";
pub const QQ_DEFAULT_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// Every field has a default, so an empty or older record loads unchanged
/// and the feature stays off.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    /// Per-event on/off by stable key (`cluster.node_down`, ...). A missing
    /// key uses the default: critical and warning on, info off.
    pub events: BTreeMap<String, bool>,
    pub cooldown_secs: u64,
    pub critical_cooldown_secs: u64,
    /// How long a down/degraded condition must persist before it is sent.
    pub confirm_secs: u64,
    /// How long a node must stay healthy before "resolved" is sent.
    pub recovery_hold_secs: u64,
    pub quorum_confirm_secs: u64,
    /// Messages produced within this window are merged into one.
    pub coalesce_secs: u64,
    /// UTC+8 "HH:MM-HH:MM". Warnings wait until the end; critical goes out.
    pub quiet_hours: Option<String>,
    /// `text` (msg_type 0) or `markdown` (msg_type 2, downgraded on refusal).
    pub style: String,
    pub no_quorum_grace_secs: u64,
    /// Never use the no-quorum fallback sender (may be silent on 2 nodes).
    pub strict_single_sender: bool,
    /// Answer the bound admin's read-only commands (status/alert/quota/help)
    /// with passive replies.
    pub commands_enabled: bool,
    /// 开播通知关注列表.
    pub watch: WatchConfig,
    pub qq: QqNotifyConfig,
}

/// Channels whose go-live sends a QQ message.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct WatchConfig {
    pub enabled: bool,
    pub channels: Vec<WatchChannel>,
}

/// Same identity bilistream uses in `channels.json`: platform plus the
/// platform's channel id (YouTube `UC…`, Twitch login, Niconico slug).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct WatchChannel {
    /// `youtube` | `twitch` | `niconico`.
    pub platform: String,
    pub channel_id: String,
    /// Display name in the message; falls back to the id.
    pub name: String,
}

pub const WATCH_PLATFORMS: [&str; 3] = ["youtube", "twitch", "niconico"];
pub const WATCH_MAX_CHANNELS: usize = 50;

impl WatchChannel {
    /// Stable identity, also the dedup variant: `youtube:UC…`. Twitch logins
    /// are case-insensitive.
    pub fn ident(&self) -> String {
        let id = self.channel_id.trim();
        if self.platform == "twitch" {
            format!("{}:{}", self.platform, id.to_ascii_lowercase())
        } else {
            format!("{}:{}", self.platform, id)
        }
    }

    pub fn display(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() {
            self.channel_id.trim()
        } else {
            name
        }
    }
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            events: BTreeMap::new(),
            cooldown_secs: 1800,
            critical_cooldown_secs: 600,
            confirm_secs: 60,
            recovery_hold_secs: 30,
            quorum_confirm_secs: 90,
            coalesce_secs: 10,
            quiet_hours: None,
            style: "text".to_string(),
            no_quorum_grace_secs: 120,
            strict_single_sender: false,
            commands_enabled: true,
            watch: WatchConfig::default(),
            qq: QqNotifyConfig::default(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct QqNotifyConfig {
    /// Not secret.
    pub app_id: String,
    /// The admin's per-bot `user_openid`. Not secret.
    pub admin_openid: String,
    /// OpenAPI and gateway host. Live-verified legacy host by default.
    pub api_base: String,
    pub token_url: String,
    /// HTTP(S) proxy for OpenAPI calls. The gateway WebSocket connects directly.
    pub proxy: Option<String>,
    /// The notify sender keeps the QQ gateway session open.
    pub keep_online: bool,
}

impl Default for QqNotifyConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            admin_openid: String::new(),
            api_base: QQ_DEFAULT_API_BASE.to_string(),
            token_url: QQ_DEFAULT_TOKEN_URL.to_string(),
            proxy: None,
            keep_online: true,
        }
    }
}

impl QqNotifyConfig {
    pub fn api_base(&self) -> &str {
        let base = self.api_base.trim().trim_end_matches('/');
        if base.is_empty() {
            QQ_DEFAULT_API_BASE
        } else {
            base
        }
    }

    pub fn token_url(&self) -> &str {
        let url = self.token_url.trim();
        if url.is_empty() {
            QQ_DEFAULT_TOKEN_URL
        } else {
            url
        }
    }
}

/// Server-side validation; the ranges match the WebUI (frontend spec §10).
pub fn validate(cfg: &NotifyConfig) -> Result<(), String> {
    let app_id = cfg.qq.app_id.trim();
    if !app_id.is_empty()
        && !(app_id.len() >= 5 && app_id.len() <= 20 && app_id.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("AppID 应为数字".to_string());
    }
    let openid = cfg.qq.admin_openid.trim();
    if openid.len() > 128 || !openid.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("管理员 openid 只能包含字母和数字".to_string());
    }
    let ranges: [(&str, u64, u64, u64); 8] = [
        ("故障确认", cfg.confirm_secs, 0, 600),
        ("恢复确认", cfg.recovery_hold_secs, 0, 1800),
        ("重复通知冷却", cfg.cooldown_secs, 60, 86_400),
        ("严重事件冷却", cfg.critical_cooldown_secs, 60, 86_400),
        ("多数派确认", cfg.quorum_confirm_secs, 0, 1800),
        ("合并窗口", cfg.coalesce_secs, 0, 300),
        ("无多数派等待", cfg.no_quorum_grace_secs, 30, 3600),
        ("", 0, 0, 0),
    ];
    for (name, value, min, max) in ranges.iter().filter(|range| !range.0.is_empty()) {
        if value < min || value > max {
            return Err(format!("{name}超出范围（{min}–{max} 秒）"));
        }
    }
    for url in [&cfg.qq.api_base, &cfg.qq.token_url] {
        let url = url.trim();
        if !url.is_empty() && !endpoint_allowed(url) {
            return Err("请填写 https:// 开头的地址，或留空使用默认值".to_string());
        }
    }
    validate_watch(&cfg.watch)?;
    if let Some(proxy) = cfg.qq.proxy.as_deref().filter(|p| !p.trim().is_empty()) {
        if !["http://", "https://", "socks5://", "socks5h://"]
            .iter()
            .any(|scheme| proxy.trim().starts_with(scheme))
        {
            return Err("代理地址格式无效".to_string());
        }
    }
    if let Some(quiet) = cfg.quiet_hours.as_deref().filter(|q| !q.trim().is_empty()) {
        if super::policy::parse_quiet_hours(quiet).is_none() {
            return Err("免打扰时段格式应为 HH:MM-HH:MM".to_string());
        }
    }
    if !matches!(cfg.style.as_str(), "text" | "markdown") {
        return Err("消息样式只能是 text 或 markdown".to_string());
    }
    for key in cfg.events.keys() {
        if super::event::EventKind::from_key(key).is_none() {
            return Err(format!(
                "未知通知事件: {}",
                crate::logging::safe_text(key, 40)
            ));
        }
    }
    Ok(())
}

/// QQ endpoints get the AppSecret and the bearer token, so the URL is parsed:
/// no userinfo, https to any host, or plain http only to an exact loopback
/// host (the mock servers in tests).
pub fn endpoint_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    match parsed.scheme() {
        "https" => true,
        "http" => matches!(host, "127.0.0.1" | "localhost" | "[::1]"),
        _ => false,
    }
}

/// Platform and id format of one watch entry.
pub fn watch_channel_valid(channel: &WatchChannel) -> bool {
    let id = channel.channel_id.trim();
    WATCH_PLATFORMS.contains(&channel.platform.as_str())
        && !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
}

fn validate_watch(watch: &WatchConfig) -> Result<(), String> {
    if watch.channels.len() > WATCH_MAX_CHANNELS {
        return Err(format!("关注列表最多 {WATCH_MAX_CHANNELS} 个频道"));
    }
    let mut seen = std::collections::HashSet::new();
    for channel in &watch.channels {
        if !WATCH_PLATFORMS.contains(&channel.platform.as_str()) {
            return Err("关注频道的平台只能是 youtube、twitch 或 niconico".to_string());
        }
        let id = channel.channel_id.trim();
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
        {
            return Err(format!(
                "关注频道 ID 无效: {}",
                crate::logging::safe_text(id, 40)
            ));
        }
        if channel.name.chars().count() > 40 {
            return Err("关注频道名称最多 40 个字".to_string());
        }
        if !seen.insert(channel.ident()) {
            return Err(format!(
                "关注列表里有重复频道: {}",
                crate::logging::safe_text(id, 40)
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_load_from_an_empty_record_and_validate() {
        let cfg: NotifyConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, NotifyConfig::default());
        assert!(!cfg.enabled);
        assert_eq!(cfg.confirm_secs, 60);
        assert_eq!(cfg.recovery_hold_secs, 30);
        assert_eq!(cfg.cooldown_secs, 1800);
        assert!(cfg.qq.keep_online);
        assert_eq!(cfg.qq.api_base(), "https://api.sgroup.qq.com");
        assert_eq!(cfg.qq.token_url(), QQ_DEFAULT_TOKEN_URL);
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_out_of_range_and_bad_hosts() {
        let mut cfg = NotifyConfig {
            confirm_secs: 601,
            ..Default::default()
        };
        assert!(validate(&cfg).is_err());
        cfg.confirm_secs = 60;
        cfg.qq.api_base = "http://api.example".to_string();
        assert!(validate(&cfg).is_err());
        cfg.qq.api_base = "https://api.bot.qq.com".to_string();
        cfg.qq.app_id = "12ab".to_string();
        assert!(validate(&cfg).is_err());
        cfg.qq.app_id = "1234567890".to_string();
        cfg.qq.admin_openid = "78117230F50CADB8722E973772B135AB".to_string();
        validate(&cfg).unwrap();
        cfg.events.insert("cluster.nope".to_string(), true);
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn endpoints_are_parsed_not_prefix_matched() {
        assert!(endpoint_allowed(
            "https://bots.qq.com/app/getAppAccessToken"
        ));
        assert!(endpoint_allowed("http://127.0.0.1:8080/x"));
        // Userinfo trick: the real host is evil.example, and in clear text.
        assert!(!endpoint_allowed("http://127.0.0.1:1@evil.example/x"));
        assert!(!endpoint_allowed("https://user:pw@bots.qq.com/x"));
        assert!(!endpoint_allowed("http://127.0.0.1.evil.example/x"));
        assert!(!endpoint_allowed("http://evil.example/x"));
        assert!(!endpoint_allowed("ftp://bots.qq.com/x"));
        let mut cfg = NotifyConfig::default();
        cfg.qq.token_url = "http://127.0.0.1:1@evil.example/x".into();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn watch_list_is_validated() {
        let mut cfg = NotifyConfig::default();
        assert!(cfg.commands_enabled);
        assert!(!cfg.watch.enabled);
        cfg.watch.channels.push(WatchChannel {
            platform: "youtube".into(),
            channel_id: "UCabc_123-x".into(),
            name: "主频道".into(),
        });
        cfg.watch.channels.push(WatchChannel {
            platform: "twitch".into(),
            channel_id: "SomeLogin".into(),
            name: String::new(),
        });
        validate(&cfg).unwrap();
        assert_eq!(cfg.watch.channels[1].ident(), "twitch:somelogin");
        assert_eq!(cfg.watch.channels[1].display(), "SomeLogin");
        let mut dup = cfg.clone();
        dup.watch.channels.push(WatchChannel {
            platform: "twitch".into(),
            channel_id: "somelogin".into(),
            name: "x".into(),
        });
        assert!(validate(&dup).is_err());
        let mut bad = cfg.clone();
        bad.watch.channels[0].platform = "bilibili".into();
        assert!(validate(&bad).is_err());
        let mut bad = cfg.clone();
        bad.watch.channels[0].channel_id = "https://youtube.com/x".into();
        assert!(validate(&bad).is_err());
    }
}
