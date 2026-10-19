//! Channel-name watch preferences.
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

#[cfg(test)]
#[path = "watch_config_tests.rs"]
mod tests;
