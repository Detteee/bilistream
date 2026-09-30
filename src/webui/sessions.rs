//! Random, revocable browser sessions. Only token hashes reach encrypted storage.
use crate::storage::{Document, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

pub(super) const LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;
const MAX_SESSIONS: usize = 128;
const RECORD: &str = "webui-sessions";

pub(super) struct Sessions {
    store: Arc<Store>,
    password_tag: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Records {
    password_tag: Option<String>,
    tokens: HashMap<String, u64>,
}

fn decode(document: Option<Document>) -> io::Result<Records> {
    document.map_or_else(
        || Ok(Records::default()),
        |doc| serde_json::from_value(doc.value).map_err(|_| io::Error::other("登录会话记录无效")),
    )
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn token_hash(token: &str) -> Option<String> {
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| digest(token.as_bytes()))
}

impl Sessions {
    pub(super) fn open(store: Arc<Store>, password: Option<&str>) -> io::Result<Self> {
        // This tag only detects a startup password change; it is never a session ID.
        // The containing record is encrypted before binding it to SQLite.
        let password_tag =
            password.map(|p| digest(format!("bilistream-password-v2\0{p}").as_bytes()));
        let expected = password_tag.clone();
        store.transaction(move |tx| {
            let records = decode(tx.read(RECORD)?)?;
            if records.password_tag != expected {
                let records = Records {
                    password_tag: expected,
                    tokens: HashMap::new(),
                };
                tx.write(RECORD, serde_json::to_value(records)?)?;
            }
            Ok(())
        })?;
        Ok(Self {
            store,
            password_tag,
        })
    }

    pub(super) fn contains(&self, token: &str, now: u64) -> bool {
        let Some(hash) = token_hash(token) else {
            return false;
        };
        let Ok(records) = self.store.read(RECORD).and_then(decode) else {
            return false;
        };
        self.password_tag.is_some()
            && records.password_tag == self.password_tag
            && records.tokens.get(&hash).is_some_and(|until| *until > now)
    }

    pub(super) fn issue(&self, previous: Option<&str>, now: u64) -> io::Result<String> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| io::Error::other("无法生成登录会话"))?;
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let hash = digest(token.as_bytes());
        let previous = previous.and_then(token_hash);
        let expected = self.password_tag.clone();
        self.store.transaction(move |tx| {
            let mut records = decode(tx.read(RECORD)?)?;
            if expected.is_none() || records.password_tag != expected {
                return Err(io::Error::other("登录密码已更改，请重新登录"));
            }
            records
                .tokens
                .retain(|key, until| *until > now && Some(key) != previous.as_ref());
            if records.tokens.len() >= MAX_SESSIONS {
                if let Some(oldest) = records
                    .tokens
                    .iter()
                    .min_by_key(|(_, until)| *until)
                    .map(|(key, _)| key.clone())
                {
                    records.tokens.remove(&oldest);
                }
            }
            records
                .tokens
                .insert(hash, now.saturating_add(LIFETIME_SECS));
            tx.write(RECORD, serde_json::to_value(records)?)?;
            Ok(())
        })?;
        Ok(token)
    }

    pub(super) fn revoke(&self, token: &str) -> io::Result<()> {
        let Some(hash) = token_hash(token) else {
            return Ok(());
        };
        self.store.transaction(move |tx| {
            let mut records = decode(tx.read(RECORD)?)?;
            if records.tokens.remove(&hash).is_some() {
                tx.write(RECORD, serde_json::to_value(records)?)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_rotate_revoke_expire_and_survive_only_unchanged_passwords() {
        let root = std::env::temp_dir().join(format!("bilistream-sessions-{}", std::process::id()));
        let open = || Store::open(root.join("data"), root.join("key/master"), None).unwrap();
        let store = open();
        let sessions = Sessions::open(Arc::clone(&store), Some("first-password")).unwrap();
        let first = sessions.issue(None, 10).unwrap();
        let second = sessions.issue(Some(&first), 20).unwrap();
        assert_ne!(first, second);
        assert!(!sessions.contains(&first, 21));
        assert!(sessions.contains(&second, 21));
        assert!(!sessions.contains(&second, 20 + LIFETIME_SECS));
        assert!(!serde_json::to_string(&store.read(RECORD).unwrap())
            .unwrap()
            .contains(&second));
        // Internal auth records must not enter a portable backup or a legacy export.
        let backup = store.export_backup("synthetic-backup-password").unwrap();
        let other = Store::open(root.join("restored"), root.join("key/other"), None).unwrap();
        other
            .restore_backup("synthetic-backup-password", &backup)
            .unwrap();
        assert!(other.read(RECORD).unwrap().is_none());
        drop(other);
        drop(sessions);
        drop(store);
        let store = open();
        let sessions = Sessions::open(Arc::clone(&store), Some("first-password")).unwrap();
        assert!(sessions.contains(&second, 30));
        sessions.revoke(&second).unwrap();
        assert!(!sessions.contains(&second, 31));
        let third = sessions.issue(None, 40).unwrap();
        let changed = Sessions::open(Arc::clone(&store), Some("changed-password")).unwrap();
        assert!(!changed.contains(&third, 41));
        assert!(!sessions.contains(&third, 41));
        assert!(sessions.issue(None, 41).is_err());
        let fourth = changed.issue(None, 42).unwrap();
        let disabled = Sessions::open(Arc::clone(&store), None).unwrap();
        assert!(!disabled.contains(&fourth, 43));
        assert!(disabled.issue(None, 43).is_err());
        drop((disabled, changed, sessions, store));
        std::fs::remove_dir_all(root).unwrap();
    }
}
