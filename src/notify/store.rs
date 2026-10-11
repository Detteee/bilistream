//! Local store records for the notifier (design §9).
//!
//! - `notify-config`: non-secret settings (`NotifyConfig`), own revision.
//! - `notify-secrets`: `{ "qq": { "app_secret": ... } }`. Encrypted by
//!   `Store` like every non-catalog record.
//! - `notify-state`: the last ledger and send outcome, so a restart neither
//!   repeats alerts nor forgets spent budgets.
//!
//! None of them is in `storage::NAMES` (so never in backups or legacy
//! exports) or `MANAGED_RECORDS`, and nothing syncs them between nodes.

use super::config::NotifyConfig;
use super::ledger::NotifyLedger;
use crate::storage::Store;
use serde::{Deserialize, Serialize};
use std::io;
use zeroize::Zeroizing;

pub const CONFIG_RECORD: &str = "notify-config";
pub const SECRETS_RECORD: &str = "notify-secrets";
pub const STATE_RECORD: &str = "notify-state";
pub const MAX_SECRET_LEN: usize = 256;

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

pub fn load_config(store: &Store) -> io::Result<(NotifyConfig, Option<u64>)> {
    let Some(doc) = store.read(CONFIG_RECORD)? else {
        return Ok((NotifyConfig::default(), None));
    };
    let cfg = serde_json::from_value(doc.value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "通知设置记录无效"))?;
    Ok((cfg, Some(doc.revision)))
}

pub fn load_secret(store: &Store) -> io::Result<(Option<AppSecret>, Option<u64>)> {
    let Some(doc) = store.read(SECRETS_RECORD)? else {
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
        let backup = store.export_backup("synthetic backup password").unwrap();
        let restored = TempStore::new();
        restored
            .1
            .restore_backup("synthetic backup password", &backup)
            .unwrap();
        for record in [CONFIG_RECORD, SECRETS_RECORD, STATE_RECORD] {
            assert!(restored.1.read(record).unwrap().is_none(), "{record}");
            assert!(!crate::storage::NAMES.contains(&record));
        }
        // And the secret never reaches the database files in plain text.
        for entry in std::fs::read_dir(temp.0.join("data")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let bytes = std::fs::read(path).unwrap();
                assert!(!bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()));
            }
        }
    }
}
