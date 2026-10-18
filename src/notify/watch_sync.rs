//! Cluster-wide 开播通知关注列表 (contract “Watch-list cluster sync”).
//!
//! Only `watch.enabled`, `watch.channels`, `watch.migration_notice` and this
//! record travel. Other notify preferences stay on the node that saved them.
//! The value rides the existing signed openid copy as an optional field, and
//! only toward members that advertise `watch_sync`. Revision 0 with an empty
//! disabled list is “never saved”: it is not exported and cannot overwrite a
//! peer. A non-empty or enabled revision-0 list seeds revision 1.

use super::config::{validate_watch_fields, WatchChannel, WatchConfig, WatchMigrationNotice};
use super::credentials::{AcceptOutcome, SaveError, SaveFailed};
use super::store;
use crate::storage::{Store, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;

pub const WATCH_RECORD: &str = "notify-watch";

/// Ordering metadata. The list itself stays in `notify-config.watch`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchMeta {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
}

impl WatchMeta {
    /// `(watch_revision, updated_at, updated_by)`. A lower key is ignored.
    pub fn key(&self) -> (u64, u64, &str) {
        (self.revision, self.updated_at, self.updated_by.as_str())
    }
}

/// The follow list as it travels on the signed member copy. Absent when this
/// node has never saved one, or the peer does not advertise `watch_sync`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchCopy {
    pub revision: u64,
    pub enabled: bool,
    pub channels: Vec<WatchChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_notice: Option<WatchMigrationNotice>,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
}

impl std::fmt::Debug for WatchCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchCopy")
            .field("revision", &self.revision)
            .field("enabled", &self.enabled)
            .field("channels", &self.channels.len())
            .field("updated_by", &self.updated_by)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl WatchCopy {
    pub fn key(&self) -> (u64, u64, &str) {
        (self.revision, self.updated_at, self.updated_by.as_str())
    }

    fn config(&self) -> WatchConfig {
        WatchConfig {
            enabled: self.enabled,
            channels: self.channels.clone(),
            migration_notice: self.migration_notice.clone(),
        }
    }
}

/// A follow-list save already validated on the node the panel talked to.
/// The holder stores these entries and does not look them up in its own
/// `channels.json`.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchSave {
    #[serde(default)]
    pub base_revision: Option<u64>,
    pub enabled: bool,
    pub channels: Vec<WatchChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_notice: Option<WatchMigrationNotice>,
}

impl std::fmt::Debug for WatchSave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchSave")
            .field("base_revision", &self.base_revision)
            .field("enabled", &self.enabled)
            .field("channels", &self.channels.len())
            .finish()
    }
}

impl WatchSave {
    fn config(&self) -> WatchConfig {
        WatchConfig {
            enabled: self.enabled,
            channels: self.channels.clone(),
            migration_notice: self.migration_notice.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchOutcome {
    pub revision: u64,
    pub changed: bool,
}

fn bad_meta() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "关注列表同步记录无效")
}

fn fail(error: SaveError) -> io::Error {
    io::Error::other(SaveFailed(error))
}

pub fn load_meta(store: &Store) -> io::Result<WatchMeta> {
    parse_meta(store.read(WATCH_RECORD)?)
}

pub(crate) fn load_meta_tx(tx: &mut Transaction<'_>) -> io::Result<WatchMeta> {
    parse_meta(tx.read(WATCH_RECORD)?)
}

pub(crate) fn parse_meta(doc: Option<crate::storage::Document>) -> io::Result<WatchMeta> {
    match doc {
        Some(doc) => serde_json::from_value(doc.value).map_err(|_| bad_meta()),
        None => Ok(WatchMeta::default()),
    }
}

fn write_meta_tx(tx: &mut Transaction<'_>, meta: &WatchMeta) -> io::Result<()> {
    let value = serde_json::to_value(meta).map_err(|_| io::Error::other("关注列表无法保存"))?;
    tx.write(WATCH_RECORD, value)?;
    Ok(())
}

fn config_value_tx(tx: &mut Transaction<'_>) -> io::Result<Value> {
    match tx.read(store::CONFIG_RECORD)? {
        Some(doc) => Ok(doc.value),
        None => serde_json::to_value(super::config::NotifyConfig::default())
            .map_err(|_| io::Error::other("通知设置无法读取")),
    }
}

/// Read the `watch` member of a config document.
///
/// `Ok(None)` means the member is absent (a legacy document: callers keep the
/// historical default). A member that cannot be parsed is an **error**, never a
/// default: defaulting it produced an empty, disabled list that passed
/// [`structural`] and was then exported at the stored revision as a real
/// "clear", which peers accepted — one unreadable local record could wipe the
/// cluster follow list.
fn watch_of(config: &Value) -> io::Result<Option<WatchConfig>> {
    match config.get("watch") {
        None => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| {
                tracing::warn!("QQ 关注列表记录无法解析，本轮不导出: {error}");
                io::Error::other("关注列表记录无法解析")
            }),
    }
}

fn watch_tx(tx: &mut Transaction<'_>) -> io::Result<WatchConfig> {
    Ok(watch_of(&config_value_tx(tx)?)?.unwrap_or_default())
}

/// Replace only `watch` on a freshly read config so an AppID written earlier
/// in this transaction is kept.
fn write_watch_tx(tx: &mut Transaction<'_>, watch: &WatchConfig) -> io::Result<()> {
    let mut config = config_value_tx(tx)?;
    if !config.is_object() {
        config = serde_json::json!({});
    }
    config["watch"] =
        serde_json::to_value(watch).map_err(|_| io::Error::other("关注列表无法保存"))?;
    tx.write(store::CONFIG_RECORD, config)?;
    Ok(())
}

fn structural(watch: &WatchConfig) -> bool {
    validate_watch_fields(watch).is_ok()
}

fn identity_ok(updated_by: &str) -> bool {
    updated_by.len() <= 128 && !updated_by.chars().any(char::is_control)
}

/// Revision 0 and a non-empty or enabled list becomes revision 1. An empty
/// disabled list stays revision 0 and is not a seed. Channel names are not
/// written into a migration notice.
pub fn seed_if_needed(store: &Store, node: &str, now: u64) -> io::Result<bool> {
    let node = node.to_string();
    store.transaction(move |tx| {
        let meta = load_meta_tx(tx)?;
        if meta.revision > 0 {
            return Ok(false);
        }
        let watch = watch_tx(tx)?;
        if !watch.enabled && watch.channels.is_empty() {
            return Ok(false);
        }
        write_meta_tx(
            tx,
            &WatchMeta {
                revision: 1,
                updated_by: node,
                updated_at: now,
                ..WatchMeta::default()
            },
        )?;
        Ok(true)
    })
}

pub(crate) fn export_snapshot(
    snapshot: &std::collections::HashMap<String, crate::storage::Document>,
) -> io::Result<Option<WatchCopy>> {
    let meta = match snapshot.get(WATCH_RECORD) {
        Some(doc) => serde_json::from_value(doc.value.clone()).map_err(|_| bad_meta())?,
        None => WatchMeta::default(),
    };
    if meta.revision == 0 {
        return Ok(None);
    }
    let watch = match snapshot.get(store::CONFIG_RECORD) {
        // An absent member keeps the historical default; an unreadable one is
        // an error (`?`) and stops the export.
        Some(doc) => watch_of(&doc.value)?.unwrap_or_default(),
        None => WatchConfig::default(),
    };
    if !structural(&watch) || !identity_ok(&meta.updated_by) {
        return Ok(None);
    }
    Ok(Some(WatchCopy {
        revision: meta.revision,
        enabled: watch.enabled,
        channels: watch.channels,
        migration_notice: watch.migration_notice,
        updated_by: meta.updated_by,
        updated_at: meta.updated_at,
    }))
}

/// Adopt a greater follow list inside the openid copy's transaction.
/// A revision-0 copy and a structurally unusable list are ignored, so they
/// cannot block the openid or the credential set. Configured names are
/// stored as sent; this node may not have their `channels.json` row yet.
/// When two revision-1 seeds meet, the gateway holder keeps its list and
/// restamps revision 2 so the rest of the cluster follows it. With no
/// holder, `(updated_at, updated_by)` already breaks the tie.
pub(crate) fn accept_tx(
    tx: &mut Transaction<'_>,
    copy: &WatchCopy,
    holder: Option<&str>,
    now: u64,
) -> io::Result<AcceptOutcome> {
    if copy.revision == 0 || !identity_ok(&copy.updated_by) || !structural(&copy.config()) {
        return Ok(AcceptOutcome::default());
    }
    let meta = load_meta_tx(tx)?;
    let local = watch_tx(tx)?;
    let seeds_meet = meta.revision == 1 && copy.revision == 1 && meta.updated_by != copy.updated_by;
    if let (true, Some(local_id)) = (seeds_meet, holder) {
        if local == copy.config() {
            return Ok(AcceptOutcome::default());
        }
        write_meta_tx(
            tx,
            &WatchMeta {
                revision: 2,
                updated_by: local_id.to_string(),
                updated_at: now.max(meta.updated_at),
            },
        )?;
        return Ok(AcceptOutcome {
            applied: false,
            restamped: true,
        });
    }
    if copy.key() <= meta.key() {
        return Ok(AcceptOutcome::default());
    }
    write_watch_tx(tx, &copy.config())?;
    write_meta_tx(
        tx,
        &WatchMeta {
            revision: copy.revision,
            updated_by: copy.updated_by.clone(),
            updated_at: copy.updated_at,
        },
    )?;
    Ok(AcceptOutcome {
        applied: true,
        restamped: false,
    })
}

/// Commit a panel save. The revision bumps only when the list changes.
/// Credential generation and the openid revision are not touched.
pub(crate) fn apply_save_tx(
    tx: &mut Transaction<'_>,
    save: &WatchSave,
    node: &str,
    now: u64,
) -> io::Result<WatchOutcome> {
    let Some(base) = save.base_revision else {
        return Err(fail(SaveError::WatchBaseRequired));
    };
    let next = save.config();
    if !structural(&next) || !identity_ok(node) {
        return Err(fail(SaveError::Invalid));
    }
    let meta = load_meta_tx(tx)?;
    if base != meta.revision {
        return Err(fail(SaveError::WatchConflict {
            current: meta.revision,
        }));
    }
    let current = watch_tx(tx)?;
    if current == next {
        return Ok(WatchOutcome {
            revision: meta.revision,
            changed: false,
        });
    }
    write_watch_tx(tx, &next)?;
    let revision = meta.revision + 1;
    write_meta_tx(
        tx,
        &WatchMeta {
            revision,
            updated_by: node.to_string(),
            updated_at: now.max(meta.updated_at),
        },
    )?;
    Ok(WatchOutcome {
        revision,
        changed: true,
    })
}

pub fn apply_save(
    store: &Store,
    save: &WatchSave,
    node: &str,
    now: u64,
) -> Result<WatchOutcome, SaveError> {
    // The seed has to commit on its own. Folding it into the save transaction
    // would roll it back on a stale base and then let that base replace the list.
    seed_if_needed(store, node, now).map_err(|_| SaveError::Store)?;
    let save = save.clone();
    let node = node.to_string();
    store
        .transaction(move |tx| apply_save_tx(tx, &save, &node, now))
        .map_err(|error| super::credentials::save_failed(&error).unwrap_or(SaveError::Store))
}

#[cfg(test)]
#[path = "watch_sync_tests.rs"]
mod tests;
