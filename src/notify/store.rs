//! Local store records for the notifier (design §9).
//!
//! - `notify-config`: non-secret settings (`NotifyConfig`), own revision.
//! - `notify-secrets`: `{ "qq": { "app_secret": ... } }`. Encrypted by
//!   `Store` like every non-catalog record.
//! - `notify-openid`: the admin openid, write-only toward the browser and
//!   sealed by `Store` with its own per-record context, next to the AppSecret.
//!   `{ admin_openid, bound_at, via, revision, writer }`; `revision` is the
//!   logical `openid_revision` (monotonic: bind, change and unbind bump it).
//!   A plain `qq.admin_openid` left in `notify-config` by an older build is
//!   moved here in one `Store::transaction` on the first load (and in the
//!   restore transaction), see `migrate_plain_openid`.
//! - `notify-state`: the last ledger and send outcome, so a restart neither
//!   repeats alerts nor forgets spent budgets.
//!
//! None of them is in `storage::NAMES` (so never in backups or legacy
//! exports) or `MANAGED_RECORDS`. AppID, AppSecret and binding metadata
//! synchronize only through the notifier's signed member channel.

use super::config::NotifyConfig;
use super::ledger::NotifyLedger;
use crate::storage::{Document, Store, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use zeroize::Zeroizing;

pub const CONFIG_RECORD: &str = "notify-config";
pub const SECRETS_RECORD: &str = "notify-secrets";
pub const OPENID_RECORD: &str = "notify-openid";
pub const STATE_RECORD: &str = "notify-state";
pub const MAX_SECRET_LEN: usize = 256;

/// Config, secret, recipient and association must be consumed from one
/// committed generation; individual reads are only for single-record work.
pub(crate) fn settings_records(
    store: &Store,
) -> io::Result<std::collections::HashMap<String, Document>> {
    let names = [
        CONFIG_RECORD,
        SECRETS_RECORD,
        OPENID_RECORD,
        super::credentials::CREDENTIALS_RECORD,
    ];
    let mut records = store.read_many(&names)?;
    if records.get(CONFIG_RECORD).is_some_and(has_plain_openid) {
        migrate_plain_openid(store)?;
        records = store.read_many(&names)?;
    }
    Ok(records)
}

pub struct SettingsSnapshot {
    pub config: (NotifyConfig, Option<u64>),
    pub secret: (Option<AppSecret>, Option<u64>),
    pub openid: OpenidState,
    pub credentials: super::credentials::CredentialsMeta,
}

pub fn load_settings(store: &Store) -> io::Result<SettingsSnapshot> {
    let mut records = settings_records(store)?;
    Ok(SettingsSnapshot {
        config: parse_config(records.remove(CONFIG_RECORD))?,
        secret: parse_secret(records.remove(SECRETS_RECORD))?,
        openid: parse_openid(records.remove(OPENID_RECORD))?,
        credentials: super::credentials::parse_meta(
            records.remove(super::credentials::CREDENTIALS_RECORD),
        )?,
    })
}

/// The QQ AppSecret. Never printed, zeroed on drop.
#[derive(Clone)]
pub struct AppSecret(Zeroizing<String>);

impl AppSecret {
    pub fn new(value: &str) -> Self {
        Self(Zeroizing::new(value.to_string()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AppSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl PartialEq for AppSecret {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SecretsDoc {
    #[serde(default)]
    qq: QqSecrets,
}

#[derive(Default, Serialize, Deserialize)]
struct QqSecrets {
    #[serde(default)]
    app_secret: String,
}

impl Drop for QqSecrets {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.app_secret);
    }
}

/// The admin openid. Never printed, zeroed on drop.
#[derive(Clone)]
pub struct AdminOpenid(Zeroizing<String>);

impl AdminOpenid {
    pub fn new(value: &str) -> Self {
        Self(Zeroizing::new(value.to_string()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AdminOpenid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("openid=<set>")
    }
}

impl PartialEq for AdminOpenid {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

/// What `notify-openid` says. Only booleans, times and the revision ever
/// leave the process; the value goes to QQ and nowhere else.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpenidState {
    pub openid: Option<AdminOpenid>,
    pub bound_at: Option<u64>,
    pub via: Option<String>,
    /// Logical `openid_revision`: 0 = never written.
    pub revision: u64,
    /// Node that assigned `revision` (partition tie-break, 1b).
    pub writer: String,
    /// Store revision of the record (change detection only).
    pub record_revision: Option<u64>,
}

impl OpenidState {
    pub fn is_set(&self) -> bool {
        self.openid.is_some()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct OpenidDoc {
    #[serde(default)]
    admin_openid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bound_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    #[serde(default)]
    revision: u64,
    /// Node that assigned `revision` (partition tie-break in 1b).
    #[serde(default)]
    writer: String,
}

impl Drop for OpenidDoc {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.admin_openid);
    }
}

fn parse_openid(doc: Option<Document>) -> io::Result<OpenidState> {
    let Some(doc) = doc else {
        return Ok(OpenidState::default());
    };
    let record_revision = Some(doc.revision);
    let parsed: OpenidDoc = serde_json::from_value(doc.value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "管理员 openid 记录无效"))?;
    let openid = (!parsed.admin_openid.is_empty()).then(|| AdminOpenid::new(&parsed.admin_openid));
    Ok(OpenidState {
        bound_at: openid.as_ref().and(parsed.bound_at),
        via: openid.as_ref().and(parsed.via.clone()),
        openid,
        revision: parsed.revision,
        writer: parsed.writer.clone(),
        record_revision,
    })
}

/// A corrupt record is an error, never a quiet `openid_set:false`.
pub fn load_openid(store: &Store) -> io::Result<OpenidState> {
    parse_openid(store.read(OPENID_RECORD)?)
}

/// A stale `expected_*_revision`: the API answers 409 with
/// `data: {code: "revision_conflict", current_revision}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionConflict {
    /// `config`, `secret` or `openid`.
    pub record: &'static str,
    pub current: u64,
}

impl std::fmt::Display for RevisionConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("配置已被其他操作修改，请重新加载后再保存")
    }
}

impl std::error::Error for RevisionConflict {}

fn conflict(record: &'static str, current: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        RevisionConflict { record, current },
    )
}

/// The conflict inside an error from `save_settings` / `write_openid`.
pub fn revision_conflict(error: &io::Error) -> Option<RevisionConflict> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<RevisionConflict>())
        .copied()
}

/// What a write does to the admin openid.
pub enum OpenidChange {
    Set {
        value: Zeroizing<String>,
        /// `manual`, `message` or `friend_add`.
        via: &'static str,
    },
    Clear,
}

/// One openid write, assigned by the single writer.
pub struct OpenidWrite {
    pub change: OpenidChange,
    /// Logical revision the caller saw; `None` = no check (pairing).
    pub expected: Option<u64>,
    pub writer: String,
    pub now: u64,
}

/// Inside a transaction: check, then write `notify-openid` with
/// `revision = current + 1`. Clearing an unset openid changes nothing.
/// Returns the logical revision afterwards.
fn write_openid_tx(tx: &mut Transaction<'_>, write: &OpenidWrite) -> io::Result<u64> {
    write_openid_tx_with_generation(tx, write, true)
}

fn write_openid_tx_with_generation(
    tx: &mut Transaction<'_>,
    write: &OpenidWrite,
    bump_credentials: bool,
) -> io::Result<u64> {
    let current = parse_openid(tx.read(OPENID_RECORD)?)?;
    if let Some(expected) = write.expected {
        if expected != current.revision {
            return Err(conflict("openid", current.revision));
        }
    }
    let doc = match &write.change {
        OpenidChange::Clear if !current.is_set() => return Ok(current.revision),
        OpenidChange::Clear => OpenidDoc {
            admin_openid: String::new(),
            bound_at: None,
            via: None,
            revision: current.revision + 1,
            writer: write.writer.clone(),
        },
        OpenidChange::Set { value, via } => OpenidDoc {
            admin_openid: value.to_string(),
            bound_at: Some(write.now),
            via: Some(via.to_string()),
            revision: current.revision + 1,
            writer: write.writer.clone(),
        },
    };
    let value =
        serde_json::to_value(&doc).map_err(|_| io::Error::other("管理员 openid 无法保存"))?;
    tx.write(OPENID_RECORD, value)?;
    if bump_credentials && matches!(write.change, OpenidChange::Set { .. }) {
        // Addendum F: remember the AppID this openid is bound under.
        super::credentials::note_bind_tx(tx, &write.writer, write.now)?;
    }
    Ok(doc.revision)
}

/// Bind, a typed openid or an unbind on its own (one transaction).
pub fn write_openid(store: &Store, write: OpenidWrite) -> io::Result<u64> {
    write_openid_with(store, write, |_| Ok(()))
}

/// Recheck a gateway binding's credential snapshot in the same transaction
/// as its recipient write. An AppID/secret change must win over an event
/// received on the old connection, even if it committed after event intake.
pub(crate) fn write_openid_with<F>(store: &Store, write: OpenidWrite, before: F) -> io::Result<u64>
where
    F: FnOnce(&mut Transaction<'_>) -> io::Result<()> + Send + 'static,
{
    store.transaction(move |tx| {
        before(tx)?;
        write_openid_tx(tx, &write)
    })
}

/// Move a plain `qq.admin_openid` out of `notify-config` into the sealed
/// `notify-openid` (checklist B3). Both records change in the caller's
/// transaction, so a crash leaves the old state or the new one. An existing
/// sealed value wins; the plain copy is only removed.
pub fn migrate_plain_openid_tx(tx: &mut Transaction<'_>) -> io::Result<bool> {
    migrate_plain_openid_tx_with(tx, |_| Ok(()))
}

pub(crate) fn migrate_plain_openid_tx_with<F>(
    tx: &mut Transaction<'_>,
    between: F,
) -> io::Result<bool>
where
    F: FnOnce(&mut Transaction<'_>) -> io::Result<()>,
{
    let Some(doc) = tx.read(CONFIG_RECORD)? else {
        return Ok(false);
    };
    let mut value = doc.value;
    let Some(plain) = value
        .get_mut("qq")
        .and_then(Value::as_object_mut)
        .and_then(|qq| qq.remove("admin_openid"))
    else {
        return Ok(false);
    };
    let plain = Zeroizing::new(plain.as_str().unwrap_or_default().trim().to_string());
    let current = parse_openid(tx.read(OPENID_RECORD)?)?;
    if !current.is_set() && super::config::validate_openid(&plain).is_ok() {
        let sealed = OpenidDoc {
            admin_openid: plain.to_string(),
            bound_at: Some(doc.updated_at),
            via: Some("manual".to_string()),
            revision: current.revision + 1,
            writer: String::new(),
        };
        let sealed = serde_json::to_value(&sealed)
            .map_err(|_| io::Error::other("管理员 openid 无法保存"))?;
        tx.write(OPENID_RECORD, sealed)?;
    }
    between(tx)?;
    tx.write(CONFIG_RECORD, value)?;
    Ok(true)
}

/// Bound or explicitly unbound, as a peer copy says (batch 1b).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyState {
    Bound,
    Unbound,
}

/// The `notify-openid` record as it travels between current members on the
/// signed peer route (contract "Peer distribution"). Only on that route;
/// never in a browser response, a log or `Debug` output.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OpenidCopy {
    pub state: CopyState,
    /// Empty when `state` is `unbound`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub openid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub revision: u64,
    #[serde(default)]
    pub writer: String,
    /// Addendum F: the synced AppID/AppSecret set. Only sent to peers that
    /// advertise `credentials_sync` (older nodes refuse unknown fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<super::credentials::CredentialsCopy>,
}

impl Drop for OpenidCopy {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.openid);
    }
}

impl std::fmt::Debug for OpenidCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenidCopy")
            .field("state", &self.state)
            .field("openid", &if self.openid.is_empty() { "" } else { "<set>" })
            .field("revision", &self.revision)
            .field("writer", &self.writer)
            .field("credentials", &self.credentials)
            .finish()
    }
}

impl OpenidCopy {
    /// Partition order (contract "Single writer"): the greatest
    /// `(openid_revision, writer_node_id)` wins.
    pub fn key(&self) -> (u64, &str) {
        (self.revision, self.writer.as_str())
    }
}

const COPY_VIA: [&str; 3] = ["manual", "message", "friend_add"];

/// This node's record as a peer copy.
pub fn export_copy(store: &Store) -> io::Result<OpenidCopy> {
    export_copy_with(store, || Ok(()))
}

/// All exported fields come from one committed snapshot. `after_snapshot`
/// lets a regression commit a concurrent update after that read, before
/// projection, without making any field come from the later generation.
pub(crate) fn export_copy_with(
    store: &Store,
    after_snapshot: impl FnOnce() -> io::Result<()>,
) -> io::Result<OpenidCopy> {
    let mut snapshot = settings_records(store)?;
    after_snapshot()?;
    let state = parse_openid(snapshot.remove(OPENID_RECORD))?;
    Ok(OpenidCopy {
        state: if state.is_set() {
            CopyState::Bound
        } else {
            CopyState::Unbound
        },
        openid: state
            .openid
            .as_ref()
            .map(|id| id.expose().to_string())
            .unwrap_or_default(),
        bound_at: state.bound_at,
        via: state.via.clone(),
        revision: state.revision,
        writer: state.writer.clone(),
        credentials: super::credentials::export_snapshot(&snapshot)?,
    })
}

/// What a received copy did here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyOutcome {
    /// The copy was newer and replaced the local record.
    pub applied: bool,
    /// Local logical revision afterwards.
    pub revision: u64,
    /// Addendum F rev 2 item 12: the holder re-stamped its seed (push).
    pub restamped: bool,
}

fn invalid_copy() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "管理员 openid 副本无效")
}

/// Keep the greater of the local record and `copy` by
/// `(openid_revision, writer_node_id)`, in one transaction. An `unbound`
/// copy that wins clears the local value (unbind propagation).
pub fn accept_copy(store: &Store, copy: &OpenidCopy) -> io::Result<CopyOutcome> {
    let holder = super::local_gateway_held().then(super::local_node_id);
    let outcome = accept_copy_as(store, copy, holder.as_deref(), crate::storage::now())?;
    if outcome.restamped {
        // Rev 2 item 12: the holder kept its values; push them.
        if let (Ok(global), Some(local)) = (crate::storage::global(), holder) {
            super::openid_sync::spawn_push(global, local);
        }
    }
    Ok(outcome)
}

/// `accept_copy` with the holder role and clock passed in (tests).
pub fn accept_copy_as(
    store: &Store,
    copy: &OpenidCopy,
    holder: Option<&str>,
    now: u64,
) -> io::Result<CopyOutcome> {
    accept_copy_as_with(store, copy, holder, now, |_| Ok(()))
}

/// Apply the holder's copy with an optional local settings write in the
/// same transaction. A cache failure after a forwarded commit must roll
/// back the complete local copy, without undoing the holder's commit.
pub(crate) fn accept_copy_as_with<F>(
    store: &Store,
    copy: &OpenidCopy,
    holder: Option<&str>,
    now: u64,
    after: F,
) -> io::Result<CopyOutcome>
where
    F: FnOnce(&mut Transaction<'_>) -> io::Result<()> + Send + 'static,
{
    match copy.state {
        CopyState::Bound => {
            super::config::validate_openid(&copy.openid).map_err(|_| invalid_copy())?;
            if copy
                .via
                .as_deref()
                .is_some_and(|via| !COPY_VIA.contains(&via))
            {
                return Err(invalid_copy());
            }
        }
        CopyState::Unbound if !copy.openid.is_empty() => return Err(invalid_copy()),
        CopyState::Unbound => {}
    }
    if copy.writer.len() > 128 || copy.writer.chars().any(char::is_control) {
        return Err(invalid_copy());
    }
    let copy = copy.clone();
    let holder = holder.map(str::to_string);
    store.transaction(move |tx| {
        let current = parse_openid(tx.read(OPENID_RECORD)?)?;
        let current_appid = super::credentials::binding_appid_tx(tx, current.is_set())?;
        // Addendum F: the set rides in the same transaction, so the openid
        // and the AppID it belongs to never land apart.
        let set = match copy.credentials.as_ref() {
            Some(set) => super::credentials::accept_tx(
                tx,
                set,
                holder.as_deref(),
                copy.state == CopyState::Bound,
                now,
            )?,
            None => Default::default(),
        };
        let credentials = set.applied;
        let restamped = set.restamped;
        // revision 0 = "never written": nothing to adopt.
        let outcome =
            if copy.revision == 0 || copy.key() <= (current.revision, current.writer.as_str()) {
                // New credentials cannot relabel a retained, higher-revision
                // openid as belonging to their AppID. A mismatch is rebind.
                super::credentials::associate_binding_tx(tx, &current_appid)?;
                CopyOutcome {
                    applied: credentials,
                    revision: current.revision,
                    restamped,
                }
            } else {
                let bound = copy.state == CopyState::Bound;
                let doc = OpenidDoc {
                    admin_openid: if bound {
                        copy.openid.clone()
                    } else {
                        String::new()
                    },
                    bound_at: if bound { copy.bound_at } else { None },
                    via: if bound { copy.via.clone() } else { None },
                    revision: copy.revision,
                    writer: copy.writer.clone(),
                };
                let value = serde_json::to_value(&doc)
                    .map_err(|_| io::Error::other("管理员 openid 无法保存"))?;
                tx.write(OPENID_RECORD, value)?;
                let appid = copy
                    .credentials
                    .as_ref()
                    .filter(|set| set.usable())
                    .map(|set| set.openid_appid.as_str())
                    .unwrap_or_default();
                // Even an older credential generation can carry the winning
                // binding revision. Keep that binding's actual association.
                // A legacy bound copy has no association proof: on F data it
                // stays unready until an explicit bind confirms the AppID.
                super::credentials::associate_binding_tx(tx, appid)?;
                CopyOutcome {
                    applied: true,
                    revision: copy.revision,
                    restamped,
                }
            };
        after(tx)?;
        Ok(outcome)
    })
}

/// Addendum F: an AppID change clears the openid in the same transaction
/// (and the same set generation). Bumps `openid_revision`.
pub(crate) fn clear_openid_tx(tx: &mut Transaction<'_>, writer: &str, now: u64) -> io::Result<u64> {
    write_openid_tx(
        tx,
        &OpenidWrite {
            change: OpenidChange::Clear,
            expected: None,
            writer: writer.to_string(),
            now,
        },
    )
}

/// A node that leaves the membership deletes its copy (contract: "a
/// removed node ... deletes its local `notify-openid` when it leaves").
/// The record is reset to "never written", so after a later re-join any
/// member's copy wins. Runs inside the membership transaction.
pub fn forget_openid_tx(tx: &mut Transaction<'_>) -> io::Result<()> {
    if tx.read(OPENID_RECORD)?.is_none() {
        return Ok(());
    }
    let value = serde_json::to_value(OpenidDoc::default())
        .map_err(|_| io::Error::other("管理员 openid 无法清除"))?;
    tx.write(OPENID_RECORD, value)?;
    Ok(())
}

pub fn migrate_plain_openid(store: &Store) -> io::Result<bool> {
    store.transaction(migrate_plain_openid_tx)
}

fn has_plain_openid(doc: &Document) -> bool {
    doc.value.pointer("/qq/admin_openid").is_some()
}

pub fn load_config(store: &Store) -> io::Result<(NotifyConfig, Option<u64>)> {
    let Some(mut doc) = store.read(CONFIG_RECORD)? else {
        return Ok((NotifyConfig::default(), None));
    };
    if has_plain_openid(&doc) {
        // First load after an upgrade (or of a legacy document): seal it.
        migrate_plain_openid(store)?;
        match store.read(CONFIG_RECORD)? {
            Some(migrated) => doc = migrated,
            None => return Ok((NotifyConfig::default(), None)),
        }
    }
    parse_config(Some(doc))
}

fn parse_config(doc: Option<Document>) -> io::Result<(NotifyConfig, Option<u64>)> {
    let Some(doc) = doc else {
        return Ok((NotifyConfig::default(), None));
    };
    let cfg = serde_json::from_value(doc.value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "通知设置记录无效"))?;
    Ok((cfg, Some(doc.revision)))
}

pub fn load_secret(store: &Store) -> io::Result<(Option<AppSecret>, Option<u64>)> {
    parse_secret(store.read(SECRETS_RECORD)?)
}

fn parse_secret(doc: Option<Document>) -> io::Result<(Option<AppSecret>, Option<u64>)> {
    let Some(doc) = doc else {
        return Ok((None, None));
    };
    let revision = doc.revision;
    let parsed: SecretsDoc = serde_json::from_value(doc.value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "通知密钥记录无效"))?;
    let secret = (!parsed.qq.app_secret.is_empty()).then(|| AppSecret::new(&parsed.qq.app_secret));
    Ok((secret, Some(revision)))
}

/// Save settings. With `expected`, fails with `WouldBlock` when the record
/// changed since the caller read it (the API maps that to 409).
pub fn save_config(store: &Store, cfg: &NotifyConfig, expected: Option<u64>) -> io::Result<u64> {
    let value = serde_json::to_value(cfg).map_err(|_| io::Error::other("通知设置无法保存"))?;
    match expected {
        Some(expected) => store.compare_exchange(CONFIG_RECORD, expected, value),
        None => store.write(CONFIG_RECORD, value),
    }
}

/// Replace (`Some`) or clear (`None`) the AppSecret.
pub fn save_secret(store: &Store, secret: Option<&str>, expected: Option<u64>) -> io::Result<u64> {
    let doc = SecretsDoc {
        qq: QqSecrets {
            app_secret: secret.unwrap_or_default().to_string(),
        },
    };
    let value = serde_json::to_value(&doc).map_err(|_| io::Error::other("通知密钥无法保存"))?;
    match expected {
        Some(expected) => store.compare_exchange(SECRETS_RECORD, expected, value),
        None => store.write(SECRETS_RECORD, value),
    }
}

/// What a settings save does to the AppSecret.
pub enum SecretChange {
    Keep,
    Set(Zeroizing<String>),
    Clear,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SavedRevisions {
    pub config: Option<u64>,
    pub secret: Option<u64>,
    pub openid: Option<u64>,
}

pub(crate) fn secret_value(secret: Option<&str>) -> io::Result<Value> {
    let doc = SecretsDoc {
        qq: QqSecrets {
            app_secret: secret.unwrap_or_default().to_string(),
        },
    };
    serde_json::to_value(&doc).map_err(|_| io::Error::other("通知密钥无法保存"))
}

fn cas(
    tx: &mut Transaction<'_>,
    record: &'static str,
    name: &str,
    expected: u64,
    value: Value,
) -> io::Result<u64> {
    let current = tx.read(name)?.map(|doc| doc.revision).unwrap_or(0);
    if current != expected {
        return Err(conflict(record, current));
    }
    tx.write(name, value)
}

/// Save `notify-config`, `notify-secrets` and/or `notify-openid` in ONE
/// Store transaction (review L7): all land or none does. Each record is
/// checked against the revision the caller loaded (0 = absent), so a
/// concurrent writer gives a `RevisionConflict` (409) with nothing applied.
pub fn save_settings(
    store: &Store,
    config: Option<(&NotifyConfig, u64)>,
    secret: SecretChange,
    expected_secret: u64,
    openid: Option<OpenidWrite>,
) -> io::Result<SavedRevisions> {
    save_settings_with(store, config, secret, expected_secret, openid, |_| Ok(()))
}

/// `between` runs after the config write and before the secret and openid
/// writes. Production passes a no-op; tests inject a crash or a racer there.
pub(crate) fn save_settings_with<F>(
    store: &Store,
    config: Option<(&NotifyConfig, u64)>,
    secret: SecretChange,
    expected_secret: u64,
    openid: Option<OpenidWrite>,
    between: F,
) -> io::Result<SavedRevisions>
where
    F: FnOnce(&mut Transaction<'_>) -> io::Result<()> + Send + 'static,
{
    save_all_with(
        store,
        config,
        secret,
        expected_secret,
        openid,
        None,
        between,
        |_| Ok(()),
    )
    .map(|(saved, _)| saved)
}

/// Addendum F credential part of a settings save: the request, the
/// committing node and the clock.
pub type CredentialsStep = (super::credentials::CredentialSave, String, u64);

/// Addendum F rev 2 item 15: config, AppSecret, the synced AppID/AppSecret
/// set (with the openid clearing an AppID change causes) and the openid
/// write commit in ONE transaction under one generation, or nothing does.
/// `between` runs after the config write; `after` runs last (tests inject
/// a failure there). A credentials refusal carries `SaveFailed`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn save_all_with<F, G>(
    store: &Store,
    config: Option<(&NotifyConfig, u64)>,
    secret: SecretChange,
    expected_secret: u64,
    openid: Option<OpenidWrite>,
    credentials: Option<CredentialsStep>,
    between: F,
    after: G,
) -> io::Result<(SavedRevisions, Option<super::credentials::SaveOutcome>)>
where
    F: FnOnce(&mut Transaction<'_>) -> io::Result<()> + Send + 'static,
    G: FnOnce(&mut Transaction<'_>) -> io::Result<()> + Send + 'static,
{
    let config = match config {
        Some((cfg, expected)) => Some((
            serde_json::to_value(cfg).map_err(|_| io::Error::other("通知设置无法保存"))?,
            expected,
        )),
        None => None,
    };
    let secret = match secret {
        SecretChange::Keep => None,
        SecretChange::Set(value) => Some(secret_value(Some(&value))?),
        SecretChange::Clear => Some(secret_value(None)?),
    };
    store.transaction(move |tx| {
        let mut saved = SavedRevisions::default();
        if let Some((value, expected)) = config {
            saved.config = Some(cas(tx, "config", CONFIG_RECORD, expected, value)?);
        }
        between(tx)?;
        let binding = openid
            .as_ref()
            .is_some_and(|write| matches!(write.change, OpenidChange::Set { .. }));
        let outcome = match credentials.as_ref() {
            Some((save, node, now)) => Some(super::credentials::apply_save_tx_with_binding(
                tx, save, node, *now, binding,
            )?),
            None => None,
        };
        if let Some(value) = secret {
            saved.secret = Some(cas(tx, "secret", SECRETS_RECORD, expected_secret, value)?);
        }
        if let Some(write) = openid.as_ref() {
            let bump_credentials = !outcome.as_ref().is_some_and(|outcome| outcome.changed);
            saved.openid = Some(write_openid_tx_with_generation(
                tx,
                write,
                bump_credentials,
            )?);
        }
        after(tx)?;
        Ok((saved, outcome))
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastOutcome {
    pub ok: bool,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Node that made the attempt.
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub test: bool,
    /// `skipped_quota` for a held 开播/下播 notice (not a failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedState {
    #[serde(default)]
    pub ledger: NotifyLedger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<LastOutcome>,
    /// Fingerprint of the bot/target the sticky mode facts belong to
    /// (AppID, openid, secret revision). A mismatch at start resets them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub settings: String,
}

pub fn load_state(store: &Store) -> PersistedState {
    store
        .read(STATE_RECORD)
        .ok()
        .flatten()
        .and_then(|doc| serde_json::from_value(doc.value).ok())
        .unwrap_or_default()
}

pub fn save_state(store: &Store, state: &PersistedState) -> io::Result<u64> {
    let value = serde_json::to_value(state).map_err(|_| io::Error::other("通知状态无法保存"))?;
    store.write(STATE_RECORD, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempStore(PathBuf, std::sync::Arc<Store>);
    impl TempStore {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).unwrap();
            let root = std::env::temp_dir().join(format!(
                "bilistream-notify-store-{}",
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ));
            std::fs::create_dir_all(root.join("legacy")).unwrap();
            let store = Store::open(
                root.join("data"),
                root.join("keys/master.key"),
                Some(root.join("legacy")),
            )
            .unwrap();
            Self(root, store)
        }
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MARKER: &str = "synthetic-notify-secret-marker";

    #[test]
    fn secret_round_trips_with_revision_checks_and_redacted_debug() {
        let temp = TempStore::new();
        let store = &temp.1;
        assert!(load_secret(store).unwrap().0.is_none());
        let first = save_secret(store, Some(MARKER), Some(0)).unwrap();
        let (secret, revision) = load_secret(store).unwrap();
        assert_eq!(secret.as_ref().map(AppSecret::expose), Some(MARKER));
        assert_eq!(revision, Some(first));
        assert_eq!(format!("{:?}", secret.unwrap()), "[redacted]");
        // A stale revision is refused.
        let error = save_secret(store, None, Some(first + 100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        save_secret(store, None, Some(first)).unwrap();
        assert!(load_secret(store).unwrap().0.is_none());
    }

    #[test]
    fn config_has_its_own_revision() {
        let temp = TempStore::new();
        let store = &temp.1;
        let (cfg, revision) = load_config(store).unwrap();
        assert_eq!(cfg, NotifyConfig::default());
        assert_eq!(revision, None);
        let mut cfg = cfg;
        cfg.enabled = true;
        let saved = save_config(store, &cfg, Some(0)).unwrap();
        // Unrelated records do not invalidate the notify revision.
        store
            .write("config.json", crate::storage::tests::config_fixture())
            .unwrap();
        let (loaded, revision) = load_config(store).unwrap();
        assert!(loaded.enabled);
        assert_eq!(revision, Some(saved));
        save_config(store, &loaded, revision).unwrap();
    }

    #[test]
    fn notify_records_stay_out_of_backups() {
        let temp = TempStore::new();
        let store = &temp.1;
        store
            .write("config.json", crate::storage::tests::config_fixture())
            .unwrap();
        save_secret(store, Some(MARKER), None).unwrap();
        save_config(store, &NotifyConfig::default(), None).unwrap();
        save_state(store, &PersistedState::default()).unwrap();
        write_openid(store, set(OPENID_MARKER, None)).unwrap();
        let backup = store.export_backup("synthetic backup password").unwrap();
        let restored = TempStore::new();
        restored
            .1
            .restore_backup("synthetic backup password", &backup)
            .unwrap();
        for record in [CONFIG_RECORD, SECRETS_RECORD, STATE_RECORD, OPENID_RECORD] {
            assert!(restored.1.read(record).unwrap().is_none(), "{record}");
            assert!(!crate::storage::NAMES.contains(&record));
        }
        // And the secret never reaches the database files in plain text.
        for entry in std::fs::read_dir(temp.0.join("data")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let bytes = std::fs::read(path).unwrap();
                assert!(!bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()));
                assert!(!bytes
                    .windows(OPENID_MARKER.len())
                    .any(|w| w == OPENID_MARKER.as_bytes()));
            }
        }
    }

    const OPENID_MARKER: &str = "SAMPLE0OPENID0VALUE0ABCDEF012345";

    fn set(value: &str, expected: Option<u64>) -> OpenidWrite {
        OpenidWrite {
            change: OpenidChange::Set {
                value: Zeroizing::new(value.to_string()),
                via: "manual",
            },
            expected,
            writer: "tokyo-1".into(),
            now: 1_791_702_240,
        }
    }

    fn plain_config(store: &Store, openid: &str) {
        let mut value = serde_json::to_value(NotifyConfig::default()).unwrap();
        value["qq"]["admin_openid"] = Value::String(openid.into());
        store
            .transaction(move |tx| tx.write_at(CONFIG_RECORD, value, 1_700_000_000))
            .unwrap();
    }

    #[test]
    fn openid_revision_is_monotonic_and_checked() {
        let temp = TempStore::new();
        let store = &temp.1;
        assert_eq!(load_openid(store).unwrap(), OpenidState::default());
        assert_eq!(write_openid(store, set(OPENID_MARKER, Some(0))).unwrap(), 1);
        let state = load_openid(store).unwrap();
        assert!(state.is_set());
        assert_eq!(state.bound_at, Some(1_791_702_240));
        assert_eq!(format!("{:?}", state.openid.unwrap()), "openid=<set>");
        let error = write_openid(store, set("OTHER", Some(0))).unwrap_err();
        assert_eq!(
            revision_conflict(&error),
            Some(RevisionConflict {
                record: "openid",
                current: 1
            })
        );
        let clear = |expected| OpenidWrite {
            change: OpenidChange::Clear,
            expected: Some(expected),
            writer: "tokyo-1".into(),
            now: 2,
        };
        assert_eq!(write_openid(store, clear(1)).unwrap(), 2);
        let state = load_openid(store).unwrap();
        assert!(!state.is_set());
        assert_eq!((state.bound_at, state.revision), (None, 2));
        // Clearing again is a no-op: the revision stays.
        assert_eq!(write_openid(store, clear(2)).unwrap(), 2);
        // A corrupt record is an error, never a quiet "unset".
        store.write(OPENID_RECORD, serde_json::json!([1])).unwrap();
        assert_eq!(
            load_openid(store).unwrap_err().to_string(),
            "管理员 openid 记录无效"
        );
    }

    #[test]
    fn plain_openid_moves_to_the_sealed_record_on_first_load() {
        let temp = TempStore::new();
        let store = &temp.1;
        plain_config(store, OPENID_MARKER);
        let (cfg, _) = load_config(store).unwrap();
        assert_eq!(cfg, NotifyConfig::default());
        let raw = store.read(CONFIG_RECORD).unwrap().unwrap();
        assert!(raw.value.pointer("/qq/admin_openid").is_none());
        let state = load_openid(store).unwrap();
        assert_eq!(state.openid.unwrap().expose(), OPENID_MARKER);
        assert_eq!(state.bound_at, Some(1_700_000_000));
        assert_eq!(state.revision, 1);
        // Idempotent: nothing left to move.
        assert!(!migrate_plain_openid(store).unwrap());
        // An existing sealed value wins over a stale plain copy.
        plain_config(store, "STALEPLAIN000000000000000000000");
        load_config(store).unwrap();
        assert_eq!(
            load_openid(store).unwrap().openid.unwrap().expose(),
            OPENID_MARKER
        );
    }

    #[test]
    fn a_failed_migration_changes_neither_record() {
        let temp = TempStore::new();
        let store = &temp.1;
        plain_config(store, OPENID_MARKER);
        let before = store.read(CONFIG_RECORD).unwrap().unwrap();
        let error = store
            .transaction(|tx| {
                migrate_plain_openid_tx_with(tx, |_| Err(io::Error::other("injected crash")))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "injected crash");
        assert!(store.read(OPENID_RECORD).unwrap().is_none());
        let after = store.read(CONFIG_RECORD).unwrap().unwrap();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.value, before.value);
    }

    #[test]
    fn restoring_a_backup_seals_a_plain_openid_left_in_the_target() {
        let source = TempStore::new();
        source
            .1
            .write("config.json", crate::storage::tests::config_fixture())
            .unwrap();
        let backup = source.1.export_backup("synthetic backup password").unwrap();
        let target = TempStore::new();
        plain_config(&target.1, OPENID_MARKER);
        target
            .1
            .restore_backup("synthetic backup password", &backup)
            .unwrap();
        let raw = target.1.read(CONFIG_RECORD).unwrap().unwrap();
        assert!(raw.value.pointer("/qq/admin_openid").is_none());
        assert_eq!(
            load_openid(&target.1).unwrap().openid.unwrap().expose(),
            OPENID_MARKER
        );
    }

    #[test]
    fn settings_save_is_all_or_nothing() {
        // Review L7: config, AppSecret and openid land together or not at all.
        let temp = TempStore::new();
        let store = &temp.1;
        let mut cfg = NotifyConfig::default();
        cfg.qq.app_id = "1234567890".into();
        let error = save_settings_with(
            store,
            Some((&cfg, 0)),
            SecretChange::Set(Zeroizing::new(MARKER.into())),
            0,
            Some(set(OPENID_MARKER, Some(0))),
            |_| Err(io::Error::other("injected crash")),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "injected crash");
        assert!(store.read(CONFIG_RECORD).unwrap().is_none());
        assert!(store.read(SECRETS_RECORD).unwrap().is_none());
        assert!(store.read(OPENID_RECORD).unwrap().is_none());

        // A concurrent secret writer between the config and secret writes:
        // the stale secret revision rolls back the config as well.
        let error = save_settings_with(
            store,
            Some((&cfg, 0)),
            SecretChange::Set(Zeroizing::new(MARKER.into())),
            0,
            None,
            |tx| {
                tx.write(SECRETS_RECORD, secret_value(Some("racer"))?)?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(revision_conflict(&error).map(|c| c.record), Some("secret"));
        assert!(store.read(CONFIG_RECORD).unwrap().is_none());
        assert!(store.read(SECRETS_RECORD).unwrap().is_none());

        // A stale config revision reports the current one.
        let saved = save_settings(store, Some((&cfg, 0)), SecretChange::Keep, 0, None).unwrap();
        let config_rev = saved.config.unwrap();
        let error = save_settings(store, Some((&cfg, 0)), SecretChange::Keep, 0, None).unwrap_err();
        assert_eq!(
            revision_conflict(&error),
            Some(RevisionConflict {
                record: "config",
                current: config_rev
            })
        );
        // All three together.
        let saved = save_settings(
            store,
            Some((&cfg, config_rev)),
            SecretChange::Set(Zeroizing::new(MARKER.into())),
            0,
            Some(set(OPENID_MARKER, Some(0))),
        )
        .unwrap();
        assert!(saved.config.is_some() && saved.secret.is_some());
        assert_eq!(saved.openid, Some(1));
        assert_eq!(load_secret(store).unwrap().0.unwrap().expose(), MARKER);
        assert!(load_openid(store).unwrap().is_set());
    }
}
