//! Notifier settings (design §9).
//!
//! Non-secret preferences live in the local store record `notify-config`
//! with their own revision, so saving them never conflicts with
//! `/api/config` edits. Like `notify-secrets`, the record is not in
//! `storage::NAMES`: it is never synced and not part of backups.
//!
//! The admin openid is not here: it is write-only toward the browser and
//! sealed in its own record `notify-openid` (`store::load_openid`). A plain
//! `qq.admin_openid` left by a pre-SQL build is reported, never migrated
//! (see `store.rs`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[path = "watch_config.rs"]
mod watch;
pub use watch::*;

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
    /// Answer the bound admin's read-only commands (status/event/quota/help)
    /// with passive replies.
    pub commands_enabled: bool,
    /// 本直播间开播/下播 notices (Addendum A/D). Absent reads as true.
    pub live_events_enabled: bool,
    /// 开播通知关注列表.
    pub watch: WatchConfig,
    pub qq: QqNotifyConfig,
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
            live_events_enabled: true,
            watch: WatchConfig::default(),
            qq: QqNotifyConfig::default(),
        }
    }
}

/// `Debug` is manual: the AppID is write-only (only whether it is set) and
/// proxy userinfo is masked.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct QqNotifyConfig {
    /// Not secret.
    pub app_id: String,
    /// OpenAPI and gateway host. Live-verified legacy host by default.
    pub api_base: String,
    pub token_url: String,
    /// HTTP(S) proxy for OpenAPI calls. The gateway WebSocket connects directly.
    pub proxy: Option<String>,
    /// The notify sender keeps the QQ gateway session open.
    pub keep_online: bool,
}

impl std::fmt::Debug for QqNotifyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QqNotifyConfig")
            .field("app_id_set", &!self.app_id.is_empty())
            .field("api_base", &self.api_base)
            .field("token_url", &self.token_url)
            .field("proxy", &crate::config::mask_proxy_opt(&self.proxy))
            .field("keep_online", &self.keep_online)
            .finish()
    }
}

impl Default for QqNotifyConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
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
    let well_formed =
        (5..=20).contains(&app_id.len()) && app_id.bytes().all(|b| b.is_ascii_digit());
    if !app_id.is_empty() && !well_formed {
        return Err("AppID 应为数字".to_string());
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
            return Err("请填写 QQ 官方 https:// 地址（*.qq.com），或留空使用默认值".to_string());
        }
    }
    validate_watch_fields(&cfg.watch).map_err(|error| error.message)?;
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

/// The admin openid typed into the panel: ASCII letters and digits, at most
/// 128. The message never quotes the input.
pub fn validate_openid(openid: &str) -> Result<(), String> {
    if openid.is_empty() || openid.len() > 128 || !openid.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err("管理员 openid 只能包含字母和数字".to_string());
    }
    Ok(())
}

/// QQ endpoints get the AppSecret and the bearer token, so the URL is parsed:
/// no userinfo, https only to a QQ host (`qq.com` or a subdomain, e.g.
/// `bots.qq.com`, `api.sgroup.qq.com`, `api.bot.qq.com`), or plain http only
/// to an exact loopback host (the mock servers in tests). A panel session
/// can therefore not point the AppSecret at its own server (review M4).
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
        "https" => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host == "qq.com" || host.ends_with(".qq.com")
        }
        "http" => matches!(host, "127.0.0.1" | "localhost" | "[::1]"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qq_config_debug_hides_the_appid_and_proxy_userinfo() {
        let cfg = QqNotifyConfig {
            app_id: "synthetic-appid-6f02".into(),
            proxy: Some("http://synthetic-qq-user:synthetic-qq-pass@proxy.example:3128".into()),
            ..Default::default()
        };
        let notify = NotifyConfig {
            qq: cfg.clone(),
            ..Default::default()
        };
        for text in [
            format!("{cfg:?}"),
            format!("{cfg:#?}"),
            format!("{notify:?}"),
        ] {
            assert!(!text.contains("synthetic-appid-6f02"), "{text}");
            assert!(text.contains("app_id_set: true"), "{text}");
            assert!(!text.contains("synthetic-qq-user"), "{text}");
            assert!(!text.contains("synthetic-qq-pass"), "{text}");
            assert!(text.contains("[redacted]@proxy.example:3128"), "{text}");
        }
    }

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
        validate(&cfg).unwrap();
        validate_openid("SAMPLE0OPENID0VALUE0ABCDEF012345").unwrap();
        assert!(validate_openid("").is_err());
        assert!(validate_openid("bad id!").is_err());
        assert!(validate_openid(&"A".repeat(129)).is_err());
        cfg.events.insert("cluster.nope".to_string(), true);
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn endpoints_are_parsed_not_prefix_matched() {
        assert!(endpoint_allowed(
            "https://bots.qq.com/app/getAppAccessToken"
        ));
        assert!(endpoint_allowed("http://127.0.0.1:8080/x"));
        // Both QQ hosts in use, plus the default API host.
        assert!(endpoint_allowed("https://api.bot.qq.com/x"));
        assert!(endpoint_allowed("https://api.sgroup.qq.com"));
        // M4: a foreign https host never receives the AppSecret.
        assert!(!endpoint_allowed(
            "https://evil.example/app/getAppAccessToken"
        ));
        assert!(!endpoint_allowed("https://qq.com.evil.example/x"));
        assert!(!endpoint_allowed("https://evilqq.com/x"));
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
        cfg.watch.channels.push(WatchChannel::Manual {
            name: "主频道".into(),
            platforms: WatchPlatforms {
                youtube: Some("UCabc_123-x".into()),
                twitch: Some("SomeLogin".into()),
                ..Default::default()
            },
        });
        validate(&cfg).unwrap();
        assert_eq!(cfg.watch.channels[0].ident(), "manual:主频道");
        let mut dup = cfg.clone();
        dup.watch.channels.push(WatchChannel::Manual {
            name: "x".into(),
            platforms: WatchPlatforms {
                twitch: Some("somelogin".into()),
                ..Default::default()
            },
        });
        assert!(validate(&dup).is_err());
        let mut bad = cfg.clone();
        bad.watch.channels[0] = WatchChannel::Manual {
            name: String::new(),
            platforms: WatchPlatforms::default(),
        };
        assert!(validate(&bad).is_err());
        let mut bad = cfg.clone();
        bad.watch.channels[0] = WatchChannel::Manual {
            name: "x".into(),
            platforms: WatchPlatforms {
                youtube: Some("https://youtube.com/x".into()),
                ..Default::default()
            },
        };
        assert!(validate(&bad).is_err());
    }
}
