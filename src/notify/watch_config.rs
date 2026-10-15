//! Channel-name watch preferences and the one-way legacy account migration.
//! The follow list is cluster-wide (`watch_sync`); every other preference in
//! this record stays on the node that saved it.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io;

pub const WATCH_PLATFORMS: [&str; 3] = ["youtube", "twitch", "niconico"];
pub const WATCH_MAX_CHANNELS: usize = 50;
const MANUAL_NAME_CHARS: usize = 40;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct WatchConfig {
    pub enabled: bool,
    pub channels: Vec<WatchChannel>,
    /// Set by migration, never accepted from a browser. A successful watch
    /// save acknowledges it; other preference or credential saves keep it.
    pub migration_notice: Option<WatchMigrationNotice>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct WatchMigrationNotice {
    pub at: u64,
    /// Number of old account rows converted to channel entries.
    pub folded: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchChannel {
    Configured {
        name: String,
    },
    /// Historical unmatched data only: unchanged retention or removal is
    /// allowed, but this entry cannot be created or actively monitored.
    Manual {
        name: String,
        platforms: WatchPlatforms,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WatchPlatforms {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub youtube: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub twitch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub niconico: Option<String>,
}

pub fn watch_account_ident(platform: &str, id: &str) -> String {
    let id = id.trim();
    if platform == "twitch" {
        format!("{platform}:{}", id.to_ascii_lowercase())
    } else {
        format!("{platform}:{id}")
    }
}

pub fn watch_account_valid(platform: &str, id: &str) -> bool {
    WATCH_PLATFORMS.contains(&platform)
        && !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
}

impl WatchPlatforms {
    pub fn accounts(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("youtube", self.youtube.as_deref()),
            ("twitch", self.twitch.as_deref()),
            ("niconico", self.niconico.as_deref()),
        ]
        .into_iter()
        .filter_map(|(platform, id)| id.map(|id| (platform, id)))
    }

    pub fn identities(&self) -> Vec<String> {
        self.accounts()
            .map(|(p, id)| watch_account_ident(p, id))
            .collect()
    }

    fn slot(&mut self, platform: &str) -> &mut Option<String> {
        match platform {
            "youtube" => &mut self.youtube,
            "twitch" => &mut self.twitch,
            "niconico" => &mut self.niconico,
            _ => unreachable!("validated watch platform"),
        }
    }

    fn normalize(&mut self) {
        for platform in WATCH_PLATFORMS {
            if let Some(id) = self.slot(platform) {
                *id = id.trim().to_string();
                if platform == "twitch" {
                    id.make_ascii_lowercase();
                }
            }
        }
    }
}

impl WatchChannel {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Configured { .. } => "configured",
            Self::Manual { .. } => "manual",
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Configured { name } | Self::Manual { name, .. } => name,
        }
    }

    pub fn ident(&self) -> String {
        format!("{}:{}", self.kind(), self.name())
    }

    pub fn normalize(&mut self) {
        if let Self::Configured { name } = self {
            *name = name.trim().to_string();
        }
        // Keep historical manual payloads byte-for-byte at the typed field
        // level, so a preference save never edits unavailable legacy data.
    }

    pub fn platforms(&self, catalog: &WatchCatalog) -> Option<WatchPlatforms> {
        match self {
            Self::Configured { name } => catalog.channels.get(name).cloned(),
            Self::Manual { platforms, .. } => Some(platforms.clone()),
        }
    }
}

/// Canonical channel records. Display aliases deliberately do not create
/// another subscription; migration finds the containing record by account.
#[derive(Debug, Clone, Default)]
pub struct WatchCatalog {
    pub channels: BTreeMap<String, WatchPlatforms>,
}

impl WatchCatalog {
    pub fn from_value(value: Option<Value>) -> io::Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let data: crate::config::ChannelsData = serde_json::from_value(value)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "频道列表无法读取"))?;
        let mut channels = BTreeMap::new();
        for channel in data.channels {
            let name = channel.name.trim();
            if name.is_empty() {
                continue;
            }
            let mut platforms = WatchPlatforms {
                youtube: channel.platforms.youtube,
                twitch: channel.platforms.twitch,
                niconico: channel.platforms.niconico,
            };
            platforms.normalize();
            for platform in WATCH_PLATFORMS {
                let slot = platforms.slot(platform);
                if slot
                    .as_deref()
                    .is_some_and(|id| !watch_account_valid(platform, id))
                {
                    *slot = None;
                }
            }
            if platforms.accounts().next().is_some() {
                channels.entry(name.to_string()).or_insert(platforms);
            }
        }
        Ok(Self { channels })
    }

    pub fn load(store: &crate::storage::Store) -> io::Result<Self> {
        Self::from_value(store.read("channels.json")?.map(|doc| doc.value))
    }

    pub fn view(&self) -> Vec<Value> {
        self.channels.iter().map(|(name, platforms)| {
            json!({"name": name, "platforms": platforms.accounts().map(|(p, _)| p).collect::<Vec<_>>()})
        }).collect()
    }

    fn containing_account(&self, ident: &str) -> Option<&str> {
        let mut matches = self.channels.iter().filter(|(_, platforms)| {
            platforms
                .accounts()
                .any(|(p, id)| watch_account_ident(p, id) == ident)
        });
        let first = matches.next()?;
        // An ambiguous roster must not silently broaden a legacy follow.
        matches.next().is_none().then_some(first.0.as_str())
    }
}

#[derive(Debug)]
pub struct WatchError {
    pub code: &'static str,
    pub message: String,
    pub field: &'static str,
}

impl WatchError {
    fn new(code: &'static str, message: &str) -> Self {
        Self {
            code,
            message: message.to_string(),
            field: "watch.channels",
        }
    }

    pub fn invalid() -> Self {
        Self::new("invalid_field", "关注频道名称或平台 ID 无效")
    }

    fn duplicate() -> Self {
        Self::new(
            "watch_duplicate",
            "该频道或平台 ID 已在关注列表中，同一个 ID 只能出现在一项里。",
        )
    }
}

pub fn validate_watch_fields(watch: &WatchConfig) -> Result<(), WatchError> {
    if watch.channels.len() > WATCH_MAX_CHANNELS {
        return Err(WatchError::new(
            "watch_too_many",
            "关注列表最多 50 项，请先移除不需要的频道。",
        ));
    }
    let mut identities = HashSet::new();
    let mut accounts = HashSet::new();
    for entry in &watch.channels {
        let name = entry.name();
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            return Err(WatchError::invalid());
        }
        if !identities.insert(entry.ident()) {
            return Err(WatchError::duplicate());
        }
        if let WatchChannel::Manual { platforms, .. } = entry {
            if name.chars().count() > MANUAL_NAME_CHARS
                || platforms.accounts().next().is_none()
                || platforms
                    .accounts()
                    .any(|(p, id)| !watch_account_valid(p, id))
            {
                return Err(WatchError::invalid());
            }
            if platforms
                .identities()
                .into_iter()
                .any(|id| !accounts.insert(id))
            {
                return Err(WatchError::duplicate());
            }
        }
    }
    Ok(())
}

pub fn validate_watch(
    watch: &WatchConfig,
    catalog: &WatchCatalog,
    current: &WatchConfig,
) -> Result<(), WatchError> {
    validate_watch_fields(watch)?;
    let mut accounts = HashSet::new();
    for entry in &watch.channels {
        if matches!(entry, WatchChannel::Manual { .. }) {
            if !current.channels.contains(entry) {
                return Err(WatchError::new(
                    "invalid_field",
                    "仅可选择频道管理中已配置的频道。历史未匹配关注项只能保留或移除。",
                ));
            }
            // Inactive compatibility data does not reserve a live account.
            continue;
        }
        let Some(platforms) = entry.platforms(catalog) else {
            if current.channels.contains(entry) {
                continue;
            }
            return Err(WatchError::new(
                "watch_unknown_channel",
                "频道管理中找不到这个频道，请刷新后重新选择。",
            ));
        };
        for ident in platforms.identities() {
            if !accounts.insert(ident) {
                return Err(WatchError::duplicate());
            }
        }
    }
    Ok(())
}

pub fn has_legacy_watch(config: &Value) -> bool {
    config
        .pointer("/watch/channels")
        .and_then(Value::as_array)
        .is_some_and(|entries| entries.iter().any(|entry| entry.get("kind").is_none()))
}

#[derive(Deserialize)]
struct LegacyChannel {
    platform: String,
    channel_id: String,
    #[serde(default)]
    name: String,
}

fn legacy_name(name: &str, id: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let name = if clean.trim().is_empty() {
        id
    } else {
        clean.trim()
    };
    name.chars().take(MANUAL_NAME_CHARS).collect()
}

fn unique_manual_name(base: &str, entries: &[WatchChannel]) -> String {
    let taken = |name: &str| {
        entries
            .iter()
            .any(|entry| matches!(entry, WatchChannel::Manual { name: known, .. } if known == name))
    };
    if !taken(base) {
        return base.to_string();
    }
    for suffix in 2.. {
        let suffix = format!(" ({suffix})");
        let prefix: String = base
            .chars()
            .take(MANUAL_NAME_CHARS - suffix.chars().count())
            .collect();
        let name = format!("{prefix}{suffix}");
        if !taken(&name) {
            return name;
        }
    }
    unreachable!()
}

/// Used both on local-store upgrade and for a stale, legacy browser save.
/// Only account identity selects a configured entry. Every unmatched ID
/// survives, including colliding names and two IDs on the same platform.
pub fn migrate_watch(
    config: &mut Value,
    catalog: &WatchCatalog,
    now: u64,
) -> Result<bool, WatchError> {
    if !has_legacy_watch(config) {
        return Ok(false);
    }
    let values = config
        .pointer("/watch/channels")
        .and_then(Value::as_array)
        .ok_or_else(WatchError::invalid)?;
    let mut entries = Vec::new();
    let mut legacy = Vec::new();
    for value in values {
        if value.get("kind").is_some() {
            entries.push(
                serde_json::from_value::<WatchChannel>(value.clone())
                    .map_err(|_| WatchError::invalid())?,
            );
        } else {
            let mut old: LegacyChannel =
                serde_json::from_value(value.clone()).map_err(|_| WatchError::invalid())?;
            old.channel_id = old.channel_id.trim().to_string();
            if !watch_account_valid(&old.platform, &old.channel_id) {
                return Err(WatchError::invalid());
            }
            if old.platform == "twitch" {
                old.channel_id.make_ascii_lowercase();
            }
            legacy.push(old);
        }
    }
    let folded = legacy.len();
    let mut seen = HashSet::new();
    for old in &legacy {
        if let Some(name) =
            catalog.containing_account(&watch_account_ident(&old.platform, &old.channel_id))
        {
            let entry = WatchChannel::Configured {
                name: name.to_string(),
            };
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
    }
    for entry in &entries {
        if let Some(platforms) = entry.platforms(catalog) {
            seen.extend(platforms.identities());
        }
    }
    for old in legacy {
        if !seen.insert(watch_account_ident(&old.platform, &old.channel_id)) {
            continue;
        }
        let base = legacy_name(&old.name, &old.channel_id);
        let existing = entries.iter_mut().find_map(|entry| match entry {
            WatchChannel::Manual { name, platforms }
                if *name == base && !platforms.accounts().any(|(p, _)| p == old.platform) =>
            {
                Some(platforms)
            }
            _ => None,
        });
        if let Some(platforms) = existing {
            *platforms.slot(&old.platform) = Some(old.channel_id);
        } else {
            let name = unique_manual_name(&base, &entries);
            let mut platforms = WatchPlatforms::default();
            *platforms.slot(&old.platform) = Some(old.channel_id);
            entries.push(WatchChannel::Manual { name, platforms });
        }
    }
    config["watch"]["channels"] =
        serde_json::to_value(entries).map_err(|_| WatchError::invalid())?;
    config["watch"]["migration_notice"] = json!({"at":now,"folded":folded});
    Ok(true)
}

#[cfg(test)]
#[path = "watch_config_tests.rs"]
mod tests;
