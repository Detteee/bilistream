//! Addendum F: the AppID, AppSecret and admin openid sync between members
//! as ONE versioned set.
//!
//! - Values stay where the worker reads them (`notify-config.qq.app_id`,
//!   `notify-secrets`, `notify-openid`); `notify-credentials` holds the set
//!   metadata `{generation, openid_appid, updated_by, updated_at}`. An
//!   accepted set writes all of them in one store transaction, so a node
//!   never applies part of a set.
//! - The set travels on 1b's openid route as `OpenidCopy.credentials`
//!   (same push retries, push-again flag, 10-minute and join pulls), only
//!   to peers whose heartbeat advertises `credentials_sync`.
//! - Saves of the AppID or AppSecret go to the gateway holder (or apply
//!   locally when there is none), which serialises them on
//!   `base_generation`: a stale base is 409 `credentials_conflict`.
//! - Changing the AppID clears the openid in the same generation; an
//!   openid whose `openid_appid` is not the current AppID is never used
//!   (`openid_rebind`).
//! - Access tokens are never part of the set.
//!
//! Nothing here formats the AppSecret or the openid: `Debug` is redacted,
//! errors are fixed strings, logs carry generations and node ids only.

use super::store::{self, MAX_SECRET_LEN};
use crate::storage::{Store, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use zeroize::Zeroizing;

pub const CREDENTIALS_RECORD: &str = "notify-credentials";

/// The set metadata on this node. `generation == 0`: never seeded (an
/// older build's data); the openid then counts as valid.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialsMeta {
    #[serde(default)]
    pub generation: u64,
    /// AppID of the last binding ("" = never bound). Retained when an
    /// AppID change clears the openid so every node can report rebind.
    #[serde(default)]
    pub openid_appid: String,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
    /// Rev 2 item 12: a generation-1 seed made while this node held the
    /// gateway. It beats any other node's seed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub holder_seed: bool,
    /// Rule 8: which `*_set` flags differed when seeds met. Flags only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_notice: Option<MigrationNotice>,
}

/// Rule 8 / rev 2 item 12: `*_set` flag names only, never a value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationNotice {
    /// Subset of `appid_set`, `appsecret_set`, `openid_set`.
    pub differed: Vec<String>,
    /// Node whose values won.
    pub winner: String,
    pub at: u64,
}

impl CredentialsMeta {
    /// Partition / tie order (rule 2, rev 2 item 12):
    /// `(generation, holder seed, updated_at, node_id)`.
    pub fn key(&self) -> (u64, bool, u64, &str) {
        (
            self.generation,
            self.holder_seed,
            self.updated_at,
            self.updated_by.as_str(),
        )
    }

    /// Rule 3: an openid bound under another AppID is invalid.
    pub fn openid_rebind(&self, openid_set: bool, appid: &str) -> bool {
        self.generation > 0
            && self.openid_appid != appid.trim()
            && (openid_set || !self.openid_appid.is_empty())
    }
}

fn bad_meta() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "通知凭据同步记录无效")
}

pub(crate) fn load_meta_tx(tx: &mut Transaction<'_>) -> io::Result<CredentialsMeta> {
    parse_meta(tx.read(CREDENTIALS_RECORD)?)
}

pub fn load_meta(store: &Store) -> io::Result<CredentialsMeta> {
    parse_meta(store.read(CREDENTIALS_RECORD)?)
}

pub(crate) fn parse_meta(doc: Option<crate::storage::Document>) -> io::Result<CredentialsMeta> {
    match doc {
        Some(doc) => serde_json::from_value(doc.value).map_err(|_| bad_meta()),
        None => Ok(CredentialsMeta::default()),
    }
}

fn write_meta_tx(tx: &mut Transaction<'_>, meta: &CredentialsMeta) -> io::Result<()> {
    let value = serde_json::to_value(meta).map_err(|_| io::Error::other("通知凭据无法保存"))?;
    tx.write(CREDENTIALS_RECORD, value)?;
    Ok(())
}

/// The set as it travels on the signed peer route. Never in a browser
/// response, a log or `Debug` output.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CredentialsCopy {
    pub generation: u64,
    pub appid: String,
    pub appsecret: String,
    #[serde(default)]
    pub openid_appid: String,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub holder_seed: bool,
    /// 清除 AppSecret: this generation has no AppSecret on purpose. Only
    /// with this flag is an empty `appsecret` a change (rule 7); the AppID
    /// and the openid stay, so a later new AppSecret needs no rebind.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub appsecret_cleared: bool,
}

impl Drop for CredentialsCopy {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.appsecret);
    }
}

impl std::fmt::Debug for CredentialsCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialsCopy")
            .field("generation", &self.generation)
            .field("appsecret", &"[redacted]")
            .field("appsecret_cleared", &self.appsecret_cleared)
            .field("updated_by", &self.updated_by)
            .finish_non_exhaustive()
    }
}

impl CredentialsCopy {
    pub fn key(&self) -> (u64, bool, u64, &str) {
        (
            self.generation,
            self.holder_seed,
            self.updated_at,
            self.updated_by.as_str(),
        )
    }

    /// Rule 7: an empty or malformed AppID/AppSecret is never a change.
    pub(crate) fn usable(&self) -> bool {
        let appid = self.appid.trim();
        self.generation > 0
            && valid_appid(appid)
            && (self.appsecret.is_empty() == self.appsecret_cleared)
            && self.appsecret.len() <= MAX_SECRET_LEN
            && !self.appsecret.chars().any(char::is_control)
            && self.updated_by.len() <= 128
            && !self.updated_by.chars().any(char::is_control)
            // A previous AppID remains here after the openid is cleared.
            // It carries the rebind cause, never a usable recipient.
            && (self.openid_appid.is_empty()
                || (self.openid_appid.len() <= 20
                    && self.openid_appid.bytes().all(|b| b.is_ascii_digit())))
    }
}

fn config_value_tx(tx: &mut Transaction<'_>) -> io::Result<Value> {
    match tx.read(store::CONFIG_RECORD)? {
        Some(doc) => Ok(doc.value),
        None => serde_json::to_value(super::config::NotifyConfig::default())
            .map_err(|_| io::Error::other("通知设置无法读取")),
    }
}

fn appid_of(config: &Value) -> String {
    config
        .pointer("/qq/app_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn set_appid(config: &mut Value, appid: &str) {
    if !config.is_object() {
        *config = Value::Object(Default::default());
    }
    let qq = config.as_object_mut().map(|o| {
        o.entry("qq")
            .or_insert_with(|| Value::Object(Default::default()))
    });
    if let Some(Value::Object(qq)) = qq {
        qq.insert("app_id".into(), Value::String(appid.to_string()));
    }
}

fn secret_tx(tx: &mut Transaction<'_>) -> io::Result<Zeroizing<String>> {
    let Some(doc) = tx.read(store::SECRETS_RECORD)? else {
        return Ok(Zeroizing::new(String::new()));
    };
    Ok(Zeroizing::new(
        doc.value
            .pointer("/qq/app_secret")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    ))
}

fn write_secret_tx(tx: &mut Transaction<'_>, secret: &str) -> io::Result<()> {
    let secret = (!secret.is_empty()).then_some(secret);
    tx.write(store::SECRETS_RECORD, store::secret_value(secret)?)?;
    Ok(())
}

fn openid_set_tx(tx: &mut Transaction<'_>) -> io::Result<bool> {
    Ok(tx
        .read(store::OPENID_RECORD)?
        .and_then(|doc| {
            doc.value
                .get("admin_openid")
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
        })
        .unwrap_or(false))
}

/// This node's set as a peer copy; `None` before it was seeded (an older
/// build's data, or nothing configured) so nothing partial is pushed.
pub fn export(store: &Store) -> io::Result<Option<CredentialsCopy>> {
    export_snapshot(&store::settings_records(store)?)
}

/// The same committed snapshot that contains the openid being exported.
/// Independent store reads can otherwise pair an old recipient with a new
/// AppID association while an atomic credential/binding save is committing.
pub(crate) fn export_snapshot(
    snapshot: &std::collections::HashMap<String, crate::storage::Document>,
) -> io::Result<Option<CredentialsCopy>> {
    let meta: CredentialsMeta = match snapshot.get(CREDENTIALS_RECORD) {
        Some(doc) => serde_json::from_value(doc.value.clone()).map_err(|_| bad_meta())?,
        None => return Ok(None),
    };
    if meta.generation == 0 {
        return Ok(None);
    }
    let appid = snapshot
        .get(store::CONFIG_RECORD)
        .map(|doc| appid_of(&doc.value))
        .unwrap_or_default();
    let appsecret = snapshot
        .get(store::SECRETS_RECORD)
        .and_then(|doc| doc.value.pointer("/qq/app_secret"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let copy = CredentialsCopy {
        generation: meta.generation,
        appid,
        appsecret_cleared: appsecret.is_empty(),
        appsecret,
        openid_appid: meta.openid_appid,
        updated_by: meta.updated_by,
        updated_at: meta.updated_at,
        holder_seed: meta.holder_seed,
    };
    Ok(copy.usable().then_some(copy))
}

/// What `accept_tx` did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcceptOutcome {
    pub applied: bool,
    /// Rev 2 item 12: this node is the gateway holder and kept its own
    /// generation-1 values against another node's seed; it re-stamped them
    /// as generation 2 and must push.
    pub restamped: bool,
}

fn flags_differ(local: (bool, bool, bool), incoming: (bool, bool, bool)) -> Vec<String> {
    let mut differed = Vec::new();
    if local.0 != incoming.0 {
        differed.push("appid_set".to_string());
    }
    if local.1 != incoming.1 {
        differed.push("appsecret_set".to_string());
    }
    if local.2 != incoming.2 {
        differed.push("openid_set".to_string());
    }
    differed
}

/// Inside the openid copy's transaction: adopt a greater set. An unusable
/// set is ignored, never an error, so it cannot block the openid part of
/// the same copy. `holder` = this node's id while it holds the gateway;
/// `incoming_openid` = the copy's openid part is bound.
pub(crate) fn accept_tx(
    tx: &mut Transaction<'_>,
    copy: &CredentialsCopy,
    holder: Option<&str>,
    incoming_openid: bool,
    now: u64,
) -> io::Result<AcceptOutcome> {
    if !copy.usable() {
        return Ok(AcceptOutcome::default());
    }
    let meta = load_meta_tx(tx)?;
    let config = config_value_tx(tx)?;
    let local_appid = appid_of(&config);
    let local_secret = secret_tx(tx)?;
    let local_flags = (
        !local_appid.is_empty(),
        !local_secret.is_empty(),
        openid_set_tx(tx)?,
    );
    let seeds_meet =
        meta.generation == 1 && copy.generation == 1 && meta.updated_by != copy.updated_by;
    if let (true, Some(local)) = (seeds_meet, holder) {
        // Rev 2 item 12: with a holder, the holder's values win.
        let same = local_appid == copy.appid.trim() && *local_secret == copy.appsecret;
        if same {
            return Ok(AcceptOutcome::default());
        }
        let differed = flags_differ(local_flags, (true, true, incoming_openid));
        write_meta_tx(
            tx,
            &CredentialsMeta {
                generation: 2,
                openid_appid: meta.openid_appid.clone(),
                updated_by: local.to_string(),
                updated_at: now.max(meta.updated_at),
                holder_seed: false,
                migration_notice: (!differed.is_empty()).then(|| MigrationNotice {
                    differed,
                    winner: local.to_string(),
                    at: now,
                }),
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
    // Migration (rule 8): this node's own seed (or unseeded local values)
    // loses; record which `*_set` flags differed, never the values.
    let migrating = meta.generation <= 1
        && copy.generation <= 2
        && meta.updated_by != copy.updated_by
        && (local_flags.0 || local_flags.1);
    let migration_notice = if migrating {
        let differed = flags_differ(local_flags, (true, true, incoming_openid));
        (!differed.is_empty()).then(|| MigrationNotice {
            differed,
            winner: copy.updated_by.clone(),
            at: now,
        })
    } else {
        meta.migration_notice.clone()
    };
    // Re-read immediately before the write. The snapshot taken for the
    // flag comparison must not put back a follow list this transaction
    // already replaced.
    let mut fresh = config_value_tx(tx)?;
    set_appid(&mut fresh, copy.appid.trim());
    tx.write(store::CONFIG_RECORD, fresh)?;
    write_secret_tx(tx, &copy.appsecret)?;
    write_meta_tx(
        tx,
        &CredentialsMeta {
            generation: copy.generation,
            openid_appid: copy.openid_appid.clone(),
            updated_by: copy.updated_by.clone(),
            updated_at: copy.updated_at,
            holder_seed: copy.holder_seed,
            migration_notice,
        },
    )?;
    Ok(AcceptOutcome {
        applied: true,
        restamped: false,
    })
}

/// Rule 8: a node with local credentials and no synced set seeds
/// generation 1. Returns whether it seeded.
/// `holder`: this node holds the gateway now (its seed then wins ties,
/// rev 2 item 12).
pub fn seed_if_needed(store: &Store, node: &str, now: u64, holder: bool) -> io::Result<bool> {
    let node = node.to_string();
    store.transaction(move |tx| {
        let meta = load_meta_tx(tx)?;
        if meta.generation > 0 {
            return Ok(false);
        }
        let config = config_value_tx(tx)?;
        let appid = appid_of(&config);
        if appid.is_empty() || secret_tx(tx)?.is_empty() {
            return Ok(false);
        }
        let openid_set = openid_set_tx(tx)?;
        write_meta_tx(
            tx,
            &CredentialsMeta {
                generation: 1,
                openid_appid: if openid_set { appid } else { String::new() },
                updated_by: node,
                updated_at: now,
                holder_seed: holder,
                migration_notice: None,
            },
        )?;
        Ok(true)
    })
}

/// A bind on the holder records the AppID it was bound under and bumps the
/// generation so the binding travels with the set. No-op before seeding.
pub(crate) fn note_bind_tx(tx: &mut Transaction<'_>, writer: &str, now: u64) -> io::Result<()> {
    let meta = load_meta_tx(tx)?;
    if meta.generation == 0 {
        return Ok(());
    }
    let appid = appid_of(&config_value_tx(tx)?);
    write_meta_tx(
        tx,
        &CredentialsMeta {
            generation: meta.generation + 1,
            openid_appid: appid,
            updated_by: writer.to_string(),
            updated_at: now.max(meta.updated_at),
            holder_seed: false,
            migration_notice: meta.migration_notice.clone(),
        },
    )
}

/// The AppID belonging to the currently selected openid, before another
/// credential generation is applied. Legacy bindings used the local AppID.
pub(crate) fn binding_appid_tx(tx: &mut Transaction<'_>, openid_set: bool) -> io::Result<String> {
    let meta = load_meta_tx(tx)?;
    if meta.generation == 0 && openid_set {
        return Ok(appid_of(&config_value_tx(tx)?));
    }
    Ok(meta.openid_appid)
}

/// Binding revision ordering is independent of credential ordering (F13).
/// Preserve the chosen binding's association without bumping or inventing
/// a credential generation. Empty + bound means unknown and requires rebind.
pub(crate) fn associate_binding_tx(tx: &mut Transaction<'_>, appid: &str) -> io::Result<()> {
    let mut meta = load_meta_tx(tx)?;
    if meta.generation > 0 && meta.openid_appid != appid {
        meta.openid_appid = appid.to_string();
        write_meta_tx(tx, &meta)?;
    }
    Ok(())
}

/// A save of the AppID and/or AppSecret (write-only; blank = unchanged).
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSave {
    /// The `credentials_generation` the page loaded. `None` only from an
    /// older panel (no check; see changelog question Q1).
    #[serde(default)]
    pub base_generation: Option<u64>,
    #[serde(default)]
    pub appid: Option<String>,
    #[serde(default)]
    pub appsecret: Option<String>,
    #[serde(default)]
    pub clear_appsecret: bool,
    /// Follow list committed with this save. Omitted for peers that do not
    /// advertise `watch_sync` (their decoder rejects unknown fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<super::watch_sync::WatchSave>,
}

impl Drop for CredentialSave {
    fn drop(&mut self) {
        if let Some(secret) = self.appsecret.as_mut() {
            zeroize::Zeroize::zeroize(secret);
        }
    }
}

impl std::fmt::Debug for CredentialSave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSave")
            .field("base_generation", &self.base_generation)
            .field("appid_changed", &self.appid.is_some())
            .field("appsecret", &self.appsecret.as_ref().map(|_| "[redacted]"))
            .field("clear_appsecret", &self.clear_appsecret)
            .field(
                "watch",
                &self.watch.as_ref().map(|watch| watch.channels.len()),
            )
            .finish()
    }
}

impl CredentialSave {
    pub fn is_change(&self) -> bool {
        self.appid.is_some()
            || self.clear_appsecret
            || self
                .appsecret
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }
}

/// What a committed save did. No values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveOutcome {
    pub generation: u64,
    pub changed: bool,
    pub openid_cleared: bool,
    /// Set when this commit also carried a follow-list save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watch_changed: bool,
}

/// Why a save was not committed. Fixed codes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    /// 409 `credentials_conflict`: stale `base_generation`, or a concurrent
    /// save committed first.
    Conflict { current: u64 },
    /// 400: malformed AppID / AppSecret.
    Invalid,
    /// 503 `credentials_holder_unreachable`: a fresh gateway holder exists
    /// but did not answer. Nothing was applied anywhere.
    HolderUnreachable,
    /// 409 `credentials_holder_moved`: the forwarded node no longer holds
    /// the gateway; the page retries.
    HolderMoved,
    /// 409 `watch_conflict`: stale `base_watch_revision`.
    WatchConflict { current: u64 },
    /// 400 `base_watch_revision_required`.
    WatchBaseRequired,
    /// 500.
    Store,
}

impl SaveError {
    pub fn code(&self) -> &'static str {
        match self {
            SaveError::Conflict { .. } => "credentials_conflict",
            SaveError::Invalid => "credentials_invalid",
            SaveError::HolderUnreachable => "credentials_holder_unreachable",
            SaveError::HolderMoved => "credentials_holder_moved",
            SaveError::WatchConflict { .. } => "watch_conflict",
            SaveError::WatchBaseRequired => "base_watch_revision_required",
            SaveError::Store => "credentials_store",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            SaveError::Conflict { .. } => "机器人凭据已被其他页面或节点修改，请重新加载后再保存",
            SaveError::Invalid => "AppID 或 AppSecret 格式无效",
            SaveError::HolderUnreachable => "暂时无法连接持有 QQ 连接的节点，请稍后再试",
            // F rev 3 rule 17: handled like the other conflicts.
            SaveError::HolderMoved => "机器人凭据已被其他页面或节点修改，请重新加载后再保存",
            SaveError::WatchConflict { .. } => "关注列表已被其他节点修改，请重新加载后再保存",
            SaveError::WatchBaseRequired => "请重新加载通知设置后再保存关注列表",
            SaveError::Store => "机器人凭据保存失败",
        }
    }
}

fn valid_appid(appid: &str) -> bool {
    (5..=20).contains(&appid.len()) && appid.bytes().all(|b| b.is_ascii_digit())
}

/// A `SaveError` carried through a store transaction so the whole
/// transaction rolls back (rev 2 item 15).
#[derive(Debug)]
pub struct SaveFailed(pub SaveError);

impl std::fmt::Display for SaveFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.message())
    }
}

impl std::error::Error for SaveFailed {}

/// The `SaveError` inside an error from a transaction that ran
/// `apply_save_tx`.
pub fn save_failed(error: &io::Error) -> Option<SaveError> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<SaveFailed>())
        .map(|failed| failed.0)
}

fn fail(error: SaveError) -> io::Error {
    io::Error::other(SaveFailed(error))
}

/// Validate the write-only values before any transaction starts.
pub fn validate_save(save: &CredentialSave) -> Result<(), SaveError> {
    if save
        .appid
        .as_deref()
        .map(str::trim)
        .is_some_and(|a| !valid_appid(a))
    {
        return Err(SaveError::Invalid);
    }
    if save
        .appsecret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(|s| s.len() > MAX_SECRET_LEN || s.chars().any(char::is_control))
    {
        return Err(SaveError::Invalid);
    }
    Ok(())
}

/// F rev 2 item 10 / rule 1: a save or 清除 without a top-level
/// `base_generation` is 400 `base_generation_required`.
pub const REQUIRE_BASE_GENERATION: bool = true;

/// True when a credential save must be refused with 400
/// `base_generation_required`. Always false while the switch is off.
pub fn base_generation_missing(save: &CredentialSave, required: bool) -> bool {
    required && save.base_generation.is_none()
}

/// Inside the caller's transaction: check `base_generation`, write the
/// AppID/AppSecret, clear the openid when the AppID changed and bump the
/// generation. A refusal is an `io::Error` carrying `SaveFailed`, so the
/// caller's whole transaction (other settings included) rolls back.
pub(crate) fn apply_save_tx(
    tx: &mut Transaction<'_>,
    save: &CredentialSave,
    node: &str,
    now: u64,
) -> io::Result<SaveOutcome> {
    apply_save_tx_with_binding(tx, save, node, now, false)
}

/// A manually supplied openid in this same transaction replaces the old
/// binding. Leave its revision alone until that write checks its CAS and
/// records the new AppID without a second credentials generation.
pub(crate) fn apply_save_tx_with_binding(
    tx: &mut Transaction<'_>,
    save: &CredentialSave,
    node: &str,
    now: u64,
    binding: bool,
) -> io::Result<SaveOutcome> {
    validate_save(save).map_err(fail)?;
    let appid = save.appid.as_deref().map(str::trim).map(str::to_string);
    let secret = save
        .appsecret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Zeroizing::new(s.to_string()));
    let clear = save.clear_appsecret && secret.is_none();
    let meta = load_meta_tx(tx)?;
    if save
        .base_generation
        .is_some_and(|base| base != meta.generation)
    {
        return Err(fail(SaveError::Conflict {
            current: meta.generation,
        }));
    }
    let config = config_value_tx(tx)?;
    let current_appid = appid_of(&config);
    let current_secret = secret_tx(tx)?;
    let next_appid = appid.unwrap_or_else(|| current_appid.clone());
    let next_secret: Zeroizing<String> = match (&secret, clear) {
        (Some(s), _) => s.clone(),
        (None, true) => Zeroizing::new(String::new()),
        (None, false) => current_secret.clone(),
    };
    let appid_changed = next_appid != current_appid;
    let secret_changed = *next_secret != *current_secret;
    if !appid_changed && !secret_changed {
        return Ok(SaveOutcome {
            generation: meta.generation,
            changed: false,
            openid_cleared: false,
            watch_revision: None,
            watch_changed: false,
        });
    }
    if appid_changed {
        // Re-read so a follow-list write earlier in this transaction is kept.
        let mut fresh = config_value_tx(tx)?;
        set_appid(&mut fresh, &next_appid);
        tx.write(store::CONFIG_RECORD, fresh)?;
    }
    if secret_changed {
        write_secret_tx(tx, &next_secret)?;
    }
    let openid_set = openid_set_tx(tx)?;
    let openid_cleared = appid_changed && openid_set && !binding;
    if openid_cleared {
        store::clear_openid_tx(tx, node, now)?;
    }
    let openid_appid = if binding {
        next_appid
    } else if openid_set && meta.generation == 0 && meta.openid_appid.is_empty() {
        current_appid
    } else {
        meta.openid_appid.clone()
    };
    let generation = meta.generation + 1;
    write_meta_tx(
        tx,
        &CredentialsMeta {
            generation,
            openid_appid,
            updated_by: node.to_string(),
            updated_at: now.max(meta.updated_at),
            holder_seed: false,
            migration_notice: meta.migration_notice.clone(),
        },
    )?;
    Ok(SaveOutcome {
        generation,
        changed: true,
        openid_cleared,
        watch_revision: None,
        watch_changed: false,
    })
}

/// A credentials-only save in its own transaction (forwarded saves).
pub fn apply_save(
    store: &Store,
    save: &CredentialSave,
    node: &str,
    now: u64,
) -> Result<SaveOutcome, SaveError> {
    validate_save(save)?;
    let save = save.clone();
    let node = node.to_string();
    store
        .transaction(move |tx| apply_save_tx(tx, &save, &node, now))
        .map_err(|error| save_failed(&error).unwrap_or(SaveError::Store))
}

/// Who commits saves right now (rule 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    Local,
    Peer(String),
    /// No fresh node holds the gateway: apply locally.
    Nobody,
}

/// The gateway holder from this node's flag and fresh peer heartbeats. A
/// stale holder is no holder.
pub fn pick_holder(
    local_id: &str,
    local_held: bool,
    nodes: &[super::observer::NodeSlot],
) -> Holder {
    if local_held {
        return Holder::Local;
    }
    nodes
        .iter()
        .filter(|n| n.id != local_id && n.fresh && n.gateway_held == Some(true))
        .map(|n| n.id.clone())
        .min()
        .map(Holder::Peer)
        .unwrap_or(Holder::Nobody)
}

/// Save on the holder: locally, or forwarded over the signed peer route.
/// A forward that does not land applies nothing anywhere.
pub async fn save(
    store: &Store,
    link: &dyn super::openid_sync::PeerLink,
    holder: &Holder,
    request: &CredentialSave,
    local: &str,
    now: u64,
) -> Result<SaveOutcome, SaveError> {
    match holder {
        Holder::Local | Holder::Nobody => apply_save(store, request, local, now),
        Holder::Peer(node) => {
            let (revision, _) = link.members().map_err(|_| SaveError::HolderUnreachable)?;
            link.forward(revision, node, request).await
        }
    }
}

/// The receiving side of a forwarded save. `holds` = this node holds the
/// gateway or no fresh node does.
pub fn serve_forward(
    store: &Store,
    from: &str,
    members: &[String],
    body: &[u8],
    holds: bool,
    local: &str,
    now: u64,
) -> Result<SaveOutcome, ForwardRefusal> {
    if from.is_empty() || !members.iter().any(|m| m == from) {
        return Err(ForwardRefusal::NotMember);
    }
    let request: CredentialSave =
        serde_json::from_slice(body).map_err(|_| ForwardRefusal::Save(SaveError::Invalid))?;
    if !holds {
        return Err(ForwardRefusal::Save(SaveError::HolderMoved));
    }
    if request.is_change() {
        validate_save(&request).map_err(ForwardRefusal::Save)?;
    }
    if request.watch.is_some() {
        // Same reason as `watch_sync::apply_save`: a base of 0 must see the
        // seeded revision, and a conflict must not undo that seed.
        super::watch_sync::seed_if_needed(store, local, now)
            .map_err(|_| ForwardRefusal::Save(SaveError::Store))?;
    }
    let local = local.to_string();
    store
        .transaction(move |tx| {
            let mut outcome = apply_save_tx(tx, &request, &local, now)?;
            if let Some(watch) = request.watch.as_ref() {
                let watch = super::watch_sync::apply_save_tx(tx, watch, &local, now)?;
                outcome.watch_revision = Some(watch.revision);
                outcome.watch_changed = watch.changed;
            }
            Ok(outcome)
        })
        .map_err(|error| ForwardRefusal::Save(save_failed(&error).unwrap_or(SaveError::Store)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardRefusal {
    NotMember,
    Save(SaveError),
}

/// `credentials_generation` etc. for responses: flags and the generation.
pub fn view(store: &Store) -> io::Result<Value> {
    Ok(view_settings(&store::load_settings(store)?))
}

pub(crate) fn view_settings(settings: &store::SettingsSnapshot) -> Value {
    let meta = &settings.credentials;
    let rebind = meta.openid_rebind(settings.openid.is_set(), &settings.config.0.qq.app_id);
    serde_json::json!({
        "credentials_generation": meta.generation,
        "openid_rebind": rebind,
        // Rule 8: flag names, the winning node and the time; no values.
        "migration_notice": meta.migration_notice,
    })
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
