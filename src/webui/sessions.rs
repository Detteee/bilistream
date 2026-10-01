//! Installation-local credentials and random sessions share one committed snapshot.
use crate::storage::{Document, Store, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io, sync::Arc};

pub(super) const LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;
pub(super) const PASSWORD_LIMIT: usize = 64 * 1024;
const MAX_SESSIONS: usize = 128;
const RECORD: &str = "webui-sessions";
const PASSWORD_RECORD: &str = "webui-password";

pub(super) struct Sessions {
    pub(super) store: Arc<Store>,
}

#[derive(Default)]
pub(super) struct PasswordSnapshot {
    pub(super) password: Option<String>,
    pub(super) revision: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    version: u8,
    password: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Records {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_tag: Option<String>, // v2 migration only; never an authority after import
    #[serde(default)]
    credential_revision: Option<u64>,
    tokens: HashMap<String, u64>,
}

fn decode(document: Option<Document>) -> io::Result<Records> {
    document.map_or_else(
        || Ok(Records::default()),
        |doc| serde_json::from_value(doc.value).map_err(|_| io::Error::other("登录会话记录无效")),
    )
}

fn password_snapshot(document: Option<Document>) -> io::Result<PasswordSnapshot> {
    let Some(doc) = document else {
        return Ok(PasswordSnapshot::default());
    };
    if !doc
        .value
        .as_object()
        .is_some_and(|object| object.contains_key("password"))
    {
        return Err(io::Error::other("面板密码记录无效"));
    }
    let credential: Credential =
        serde_json::from_value(doc.value).map_err(|_| io::Error::other("面板密码记录无效"))?;
    if credential.version != 1
        || credential.password.as_ref().is_some_and(|p| {
            normalize_password(p)
                .as_ref()
                .map_or(true, |normalized| normalized != p)
        })
    {
        return Err(io::Error::other("面板密码记录无效"));
    }
    Ok(PasswordSnapshot {
        password: credential.password,
        revision: Some(doc.revision),
    })
}

pub(super) fn normalize_password(value: &str) -> io::Result<String> {
    if value.len() > PASSWORD_LIMIT || value.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "密码不能为空或超过 64 KiB",
        ));
    }
    Ok(value.trim().to_owned())
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

pub(super) fn new_token() -> io::Result<String> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("无法生成登录会话"))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) fn conflict() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "面板密码已更改，请刷新后重试")
}

/// Prepared outside the transaction; the token is exposed only after commit.
pub(crate) struct PasswordMutation {
    expected: Option<u64>,
    password: Option<String>,
    initial_token: Option<String>,
}

impl PasswordMutation {
    pub(super) fn new(
        expected: Option<u64>,
        password: Option<String>,
        initial: bool,
    ) -> io::Result<Self> {
        Ok(Self {
            expected,
            password,
            initial_token: if initial { Some(new_token()?) } else { None },
        })
    }

    pub(crate) fn token(&self) -> Option<&str> {
        self.initial_token.as_deref()
    }

    pub(crate) fn apply(self, tx: &mut Transaction<'_>) -> io::Result<()> {
        let current = password_snapshot(tx.read(PASSWORD_RECORD)?)?;
        if current.revision != self.expected {
            return Err(conflict());
        }
        let revision = tx.write(
            PASSWORD_RECORD,
            serde_json::to_value(Credential {
                version: 1,
                password: self.password,
            })?,
        )?;
        let mut records = Records {
            credential_revision: Some(revision),
            ..Records::default()
        };
        if let Some(token) = self.initial_token {
            records.tokens.insert(
                digest(token.as_bytes()),
                crate::storage::now().saturating_add(LIFETIME_SECS),
            );
        }
        tx.write(RECORD, serde_json::to_value(records)?)?;
        Ok(())
    }
}

impl Sessions {
    pub(super) fn open(
        store: Arc<Store>,
        bootstrap: impl FnOnce() -> io::Result<Option<String>>,
    ) -> io::Result<Self> {
        let snapshot = password_snapshot(store.read(PASSWORD_RECORD)?)?;
        if snapshot.revision.is_none() {
            if let Some(password) = bootstrap()? {
                let password = normalize_password(&password)?;
                store.transaction(move |tx| {
                    if tx.read(PASSWORD_RECORD)?.is_some() {
                        return Err(conflict());
                    }
                    let mut records = decode(tx.read(RECORD)?)?;
                    let tag = digest(format!("bilistream-password-v2\0{password}").as_bytes());
                    if records.password_tag.as_deref() != Some(&tag) {
                        records.tokens.clear();
                    }
                    records.credential_revision = Some(tx.write(
                        PASSWORD_RECORD,
                        serde_json::to_value(Credential {
                            version: 1,
                            password: Some(password),
                        })?,
                    )?);
                    records.password_tag = None;
                    tx.write(RECORD, serde_json::to_value(records)?)?;
                    Ok(())
                })?;
            }
        }
        // Malformed saved sessions fail startup rather than silently disabling auth.
        decode(store.read(RECORD)?)?;
        Ok(Self { store })
    }

    pub(super) fn snapshot(&self) -> io::Result<PasswordSnapshot> {
        password_snapshot(self.store.read(PASSWORD_RECORD)?)
    }

    /// Password requirement and token validity are read under the same cache lock.
    pub(super) fn access(
        &self,
        token: Option<&str>,
        now: u64,
    ) -> io::Result<(PasswordSnapshot, bool)> {
        let mut docs = self.store.read_many(&[PASSWORD_RECORD, RECORD])?;
        let snapshot = password_snapshot(docs.remove(PASSWORD_RECORD))?;
        let records = decode(docs.remove(RECORD))?;
        let valid = snapshot.password.is_none()
            || (records.credential_revision == snapshot.revision
                && token
                    .and_then(token_hash)
                    .and_then(|hash| records.tokens.get(&hash).copied())
                    .is_some_and(|until| until > now));
        Ok((snapshot, valid))
    }

    #[cfg(test)]
    fn contains(&self, token: &str, now: u64) -> bool {
        self.access(Some(token), now)
            .is_ok_and(|(state, valid)| state.password.is_some() && valid)
    }

    pub(super) fn issue(
        &self,
        expected: Option<u64>,
        previous: Option<&str>,
        now: u64,
    ) -> io::Result<String> {
        let token = new_token()?;
        let hash = digest(token.as_bytes());
        let previous = previous.and_then(token_hash);
        self.store.transaction(move |tx| {
            let snapshot = password_snapshot(tx.read(PASSWORD_RECORD)?)?;
            let mut records = decode(tx.read(RECORD)?)?;
            if snapshot.password.is_none() || snapshot.revision != expected {
                return Err(conflict());
            }
            // The password record is the authority. A session record from another
            // generation (e.g. rewritten by an older binary after a rollback) holds
            // no valid tokens; start a fresh one rather than refusing every login.
            if records.credential_revision != expected {
                records = Records {
                    credential_revision: expected,
                    ..Records::default()
                };
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

    pub(super) fn mutate(&self, mutation: PasswordMutation) -> io::Result<()> {
        self.store.transaction(move |tx| mutation.apply(tx))
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

pub(crate) fn reset_password(store: Arc<Store>) -> io::Result<()> {
    let snapshot = password_snapshot(store.read(PASSWORD_RECORD)?)?;
    let mutation = PasswordMutation::new(snapshot.revision, None, false)?;
    store.transaction(move |tx| mutation.apply(tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_restart_revocation_conflicts_and_rollback() {
        let root = std::env::temp_dir().join(format!(
            "bilistream-password-sessions-{}",
            std::process::id()
        ));
        let open = || Store::open(root.join("data"), root.join("key/master"), None).unwrap();
        let store = open();
        let token = new_token().unwrap();
        store.write(RECORD, serde_json::json!({"password_tag": digest(b"bilistream-password-v2\0first"), "tokens": {digest(token.as_bytes()): 10000}})).unwrap();
        let sessions = Sessions::open(store.clone(), || Ok(None)).unwrap();
        assert!(sessions.snapshot().unwrap().revision.is_none());
        drop(sessions);
        let sessions = Sessions::open(store.clone(), || Ok(Some("first".into()))).unwrap();
        assert!(sessions.contains(&token, 5));
        let revision = sessions.snapshot().unwrap().revision;
        let second = sessions.issue(revision, Some(&token), 10).unwrap();
        assert!(!sessions.contains(&token, 11));
        assert!(sessions.contains(&second, 11));
        assert!(!sessions.contains(&second, 10 + LIFETIME_SECS));
        let failing = PasswordMutation::new(revision, Some("changed".into()), true).unwrap();
        let result: io::Result<()> = store.transaction(move |tx| {
            tx.write("config.json", serde_json::json!({"synthetic": true}))?;
            failing.apply(tx)?;
            Err(io::Error::other("forced transaction failure"))
        });
        assert!(result.is_err());
        assert!(store.read("config.json").unwrap().is_none());
        assert_eq!(sessions.snapshot().unwrap().revision, revision);
        assert!(sessions.contains(&second, 11));
        drop((sessions, store));
        let store = open();
        let sessions =
            Sessions::open(store.clone(), || panic!("saved state ignores bootstrap")).unwrap();
        assert!(sessions.contains(&second, 12));
        sessions
            .mutate(PasswordMutation::new(revision, Some("first".into()), false).unwrap())
            .unwrap();
        assert!(!sessions.contains(&second, 13));
        assert_eq!(
            sessions.issue(revision, None, 13).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        reset_password(store.clone()).unwrap();
        let disabled =
            Sessions::open(store.clone(), || panic!("explicit clear must not reimport")).unwrap();
        assert!(disabled.snapshot().unwrap().password.is_none());
        assert!(disabled.snapshot().unwrap().revision.is_some());
        let backup = store.export_backup("synthetic-backup-password").unwrap();
        let destination = Store::open(root.join("restored"), root.join("key/other"), None).unwrap();
        destination
            .restore_backup("synthetic-backup-password", &backup)
            .unwrap();
        assert!(destination.read(RECORD).unwrap().is_none());
        assert!(destination.read(PASSWORD_RECORD).unwrap().is_none());
        let protected = Sessions::open(destination.clone(), || {
            Ok(Some("destination-password".into()))
        })
        .unwrap();
        destination
            .restore_backup("synthetic-backup-password", &backup)
            .unwrap();
        assert_eq!(
            protected.snapshot().unwrap().password.as_deref(),
            Some("destination-password")
        );
        drop((protected, destination, disabled, sessions, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_first_creation_has_one_winner_and_old_login_cannot_issue_after_rotation() {
        let root =
            std::env::temp_dir().join(format!("bilistream-password-race-{}", std::process::id()));
        let store = Store::open(root.join("data"), root.join("key/master"), None).unwrap();
        let sessions = Arc::new(Sessions::open(store.clone(), || Ok(None)).unwrap());
        let first = PasswordMutation::new(None, Some("first".into()), true).unwrap();
        let second = PasswordMutation::new(None, Some("second".into()), true).unwrap();
        let start = Arc::new(std::sync::Barrier::new(3));
        let threads = [first, second]
            .into_iter()
            .map(|mutation| {
                let sessions = sessions.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    sessions.mutate(mutation)
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results.into_iter().find_map(Result::err).unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        let captured = sessions.snapshot().unwrap().revision;
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let login = {
            let sessions = sessions.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                // Password verification happened at captured revision; delay only session issuance.
                barrier.wait();
                sessions.issue(captured, None, crate::storage::now())
            })
        };
        sessions
            .mutate(PasswordMutation::new(captured, Some("rotated".into()), false).unwrap())
            .unwrap();
        barrier.wait();
        assert_eq!(
            login.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop((sessions, store));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn login_recovers_a_session_record_rewritten_by_an_older_binary() {
        let root = std::env::temp_dir().join(format!(
            "bilistream-password-rollback-{}",
            std::process::id()
        ));
        let store = Store::open(root.join("data"), root.join("key/master"), None).unwrap();
        let sessions = Sessions::open(store.clone(), || Ok(Some("saved".into()))).unwrap();
        let revision = sessions.snapshot().unwrap().revision;
        let before = sessions.issue(revision, None, 10).unwrap();
        // A rolled-back binary ignores webui-password and rewrites the session
        // record in its own v2 shape, without a credential revision.
        let stale = new_token().unwrap();
        store
            .write(
                RECORD,
                serde_json::json!({
                    "password_tag": digest(b"bilistream-password-v2\0saved"),
                    "tokens": {digest(stale.as_bytes()): 10000},
                }),
            )
            .unwrap();
        assert!(!sessions.contains(&before, 11));
        assert!(!sessions.contains(&stale, 11));
        let after = sessions.issue(revision, None, 12).unwrap();
        assert!(sessions.contains(&after, 13));
        assert!(!sessions.contains(&stale, 13));
        drop((sessions, store));
        std::fs::remove_dir_all(root).unwrap();
    }
}
