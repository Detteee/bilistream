use super::*;
use crate::notify::config::{WatchChannel, WatchConfig};
use crate::notify::credentials::{self, CredentialsCopy, ForwardRefusal, SaveError};
use crate::notify::openid_sync::{pull_all, push_all, RetryPolicy};
use crate::notify::store::{self, accept_copy_as, save_settings, SecretChange};
use crate::notify::test_support::{fast_policy, MemCluster, TestStore};

const SECRET: &str = "SYNTHETICwatchSECRETmarker0001";
const APPID: &str = "102030405";
const APPID2: &str = "908070605";

fn temp() -> TestStore {
    TestStore::new("watch-sync")
}

const FAST: RetryPolicy = fast_policy(4);

fn names(channels: &[WatchChannel]) -> Vec<String> {
    channels
        .iter()
        .map(|channel| channel.name().to_string())
        .collect()
}

fn saved(store: &Store) -> (bool, Vec<String>, u64) {
    let watch = store::load_config(store).unwrap().0.watch;
    let revision = load_meta(store).unwrap().revision;
    (watch.enabled, names(&watch.channels), revision)
}

fn put_watch(store: &Store, enabled: bool, list: &[&str]) {
    let (mut cfg, revision) = store::load_config(store).unwrap();
    cfg.watch = WatchConfig {
        enabled,
        channels: list
            .iter()
            .map(|name| WatchChannel::Configured {
                name: (*name).into(),
            })
            .collect(),
        migration_notice: None,
    };
    store::save_config(store, &cfg, revision).unwrap();
}

fn configure(store: &Store) {
    let mut cfg = crate::notify::NotifyConfig::default();
    cfg.qq.app_id = APPID.into();
    save_settings(
        store,
        Some((&cfg, 0)),
        SecretChange::Set(zeroize::Zeroizing::new(SECRET.into())),
        0,
        None,
    )
    .unwrap();
}

fn document(value: serde_json::Value) -> crate::storage::Document {
    crate::storage::Document {
        value,
        revision: 1,
        updated_at: 10,
    }
}

/// A record this build cannot read must stop the export, not become a
/// legitimate "cleared" list that peers accept as a real change.
#[test]
fn an_unparseable_watch_record_never_exports_a_clear() {
    let record = serde_json::json!({
        "watch": {
            "enabled": true,
            "channels": [{"kind": "future-kind", "name": "x"}],
            "migration_notice": null
        }
    });
    assert!(
        watch_of(&record).is_err(),
        "an unreadable member must not fall back to a default list"
    );

    let mut snapshot = std::collections::HashMap::new();
    snapshot.insert(
        WATCH_RECORD.to_string(),
        document(serde_json::json!({
            "revision": 3,
            "updated_by": "jp",
            "updated_at": 10
        })),
    );
    snapshot.insert(store::CONFIG_RECORD.to_string(), document(record));

    assert!(
        export_snapshot(&snapshot).is_err(),
        "a record that cannot be parsed must not export a cleared follow list"
    );
}

/// A legacy document without the member keeps the historical default, so the
/// stricter parse does not break the migration path.
#[test]
fn an_absent_watch_member_still_uses_the_default() {
    assert_eq!(watch_of(&serde_json::json!({})).unwrap(), None);
    assert_eq!(
        watch_of(&serde_json::json!({"qq": {}})).unwrap(),
        None,
        "an unrelated member is not a watch record"
    );
}

#[tokio::test]
async fn an_existing_list_beats_empty_disabled_peers_without_another_save() {
    let (jp, ca, ny, blank) = (temp(), temp(), temp(), temp());
    put_watch(&jp.1, true, &["Kamito", "橘ひなの"]);
    assert!(seed_if_needed(&jp.1, "jp", 100).unwrap());
    assert!(!seed_if_needed(&ca.1, "ca", 200).unwrap());
    assert!(!seed_if_needed(&ny.1, "ny", 300).unwrap());
    assert!(!seed_if_needed(&blank.1, "blank", 400).unwrap());
    let cluster = MemCluster::new(&[
        ("jp", jp.1.clone()),
        ("ca", ca.1.clone()),
        ("ny", ny.1.clone()),
        ("blank", blank.1.clone()),
    ]);
    push_all(&blank.1, &cluster.link("blank"), FAST)
        .await
        .unwrap();
    assert_eq!(
        saved(&jp.1),
        (true, vec!["Kamito".into(), "橘ひなの".into()], 1),
        "revision 0 empty must not overwrite"
    );
    assert!(pull_all(&ca.1, &cluster.link("ca")).await.unwrap());
    assert!(pull_all(&ny.1, &cluster.link("ny")).await.unwrap());
    assert_eq!(saved(&ca.1), saved(&jp.1));
    assert_eq!(saved(&ny.1), saved(&jp.1));
    assert!(!crate::storage::NAMES.contains(&WATCH_RECORD));
}

#[tokio::test]
async fn an_explicit_clear_replicates_and_a_holder_seed_beats_another_seed() {
    let (jp, ca) = (temp(), temp());
    put_watch(&jp.1, true, &["Kamito", "橘ひなの"]);
    put_watch(&ca.1, true, &["other"]);
    assert!(seed_if_needed(&jp.1, "jp", 100).unwrap());
    assert!(seed_if_needed(&ca.1, "ca", 500).unwrap());
    let incoming = store::export_copy(&ca.1).unwrap();
    let outcome = accept_copy_as(&jp.1, &incoming, Some("jp"), 600).unwrap();
    assert!(outcome.restamped);
    assert_eq!(
        saved(&jp.1),
        (true, vec!["Kamito".into(), "橘ひなの".into()], 2),
        "the gateway holder keeps its seed"
    );
    let cleared = apply_save(
        &jp.1,
        &WatchSave {
            base_revision: Some(2),
            enabled: false,
            channels: Vec::new(),
            migration_notice: None,
        },
        "jp",
        700,
    )
    .unwrap();
    assert!(cleared.changed);
    assert_eq!(cleared.revision, 3);
    assert_eq!(credentials::load_meta(&jp.1).unwrap().generation, 0);
    assert!(!store::load_openid(&jp.1).unwrap().is_set());
    let cluster = MemCluster::new(&[("jp", jp.1.clone()), ("ca", ca.1.clone())]);
    push_all(&jp.1, &cluster.link("jp"), FAST).await.unwrap();
    assert_eq!(saved(&ca.1), (false, Vec::new(), 3));
}

#[test]
fn a_stale_or_missing_base_changes_nothing() {
    let jp = temp();
    put_watch(&jp.1, true, &["Kamito"]);
    seed_if_needed(&jp.1, "jp", 100).unwrap();
    let before = saved(&jp.1);
    let stale = apply_save(
        &jp.1,
        &WatchSave {
            base_revision: Some(0),
            enabled: false,
            channels: Vec::new(),
            migration_notice: None,
        },
        "jp",
        200,
    );
    assert_eq!(stale, Err(SaveError::WatchConflict { current: 1 }));
    assert_eq!(stale.unwrap_err().code(), "watch_conflict");
    let missing = apply_save(
        &jp.1,
        &WatchSave {
            base_revision: None,
            enabled: false,
            channels: Vec::new(),
            migration_notice: None,
        },
        "jp",
        200,
    );
    assert_eq!(missing, Err(SaveError::WatchBaseRequired));
    assert_eq!(saved(&jp.1), before);
}

#[tokio::test]
async fn a_peer_without_watch_sync_keeps_its_own_list() {
    let (jp, ca) = (temp(), temp());
    configure(&jp.1);
    credentials::seed_if_needed(&jp.1, "jp", 100, true).unwrap();
    put_watch(&jp.1, true, &["Kamito", "橘ひなの"]);
    seed_if_needed(&jp.1, "jp", 100).unwrap();
    put_watch(&ca.1, true, &["local-only"]);
    let cluster = MemCluster::new(&[("jp", jp.1.clone()), ("ca", ca.1.clone())]);
    cluster.no_watch.lock().unwrap().insert("ca".into());
    let report = push_all(&jp.1, &cluster.link("jp"), FAST).await.unwrap();
    assert!(report.failed.is_empty());
    assert_eq!(saved(&ca.1).1, vec!["local-only".to_string()]);
    assert_eq!(
        saved(&ca.1).2,
        0,
        "the omitted field must not seed a revision"
    );
    let secret = store::load_secret(&ca.1).unwrap().0;
    assert_eq!(
        secret.as_ref().map(|s| s.expose().to_string()).as_deref(),
        Some(SECRET)
    );
    let pulled = crate::notify::openid_sync::serve_pull(
        &jp.1,
        "ca",
        &["jp".into(), "ca".into()],
        true,
        false,
    )
    .unwrap();
    assert!(pulled.watch.is_none());
    assert!(pulled.credentials.is_some());
}

#[test]
fn credential_and_watch_writes_do_not_clobber_each_other() {
    fn watch_copy(revision: u64, name: &str, by: &str) -> WatchCopy {
        WatchCopy {
            revision,
            enabled: true,
            channels: vec![WatchChannel::Configured { name: name.into() }],
            migration_notice: None,
            updated_by: by.into(),
            updated_at: 50,
        }
    }
    fn credential_copy(appid: &str) -> CredentialsCopy {
        CredentialsCopy {
            generation: 2,
            appid: appid.into(),
            appsecret: SECRET.into(),
            appsecret_cleared: false,
            openid_appid: String::new(),
            updated_by: "jp".into(),
            updated_at: 50,
            holder_seed: false,
        }
    }
    let store = temp();
    configure(&store.1);
    credentials::seed_if_needed(&store.1, "ca", 10, false).unwrap();
    put_watch(&store.1, true, &["Kamito"]);
    seed_if_needed(&store.1, "ca", 10).unwrap();
    let watch_bytes = store::load_config(&store.1).unwrap().0.watch.clone();
    store
        .1
        .transaction({
            let copy = credential_copy(APPID2);
            move |tx| {
                credentials::accept_tx(tx, &copy, None, false, 80)?;
                let watch = watch_tx(tx)?;
                assert_eq!(watch, watch_bytes);
                Ok(())
            }
        })
        .unwrap();
    assert_eq!(store::load_config(&store.1).unwrap().0.qq.app_id, APPID2);

    let other = temp();
    configure(&other.1);
    credentials::seed_if_needed(&other.1, "ca", 10, false).unwrap();
    put_watch(&other.1, true, &["Kamito"]);
    seed_if_needed(&other.1, "ca", 10).unwrap();
    let generation = credentials::load_meta(&other.1).unwrap().generation;
    other
        .1
        .transaction({
            let watch = watch_copy(2, "橘ひなの", "jp");
            let credentials = credential_copy(APPID2);
            move |tx| {
                accept_tx(tx, &watch, None, 80)?;
                credentials::accept_tx(tx, &credentials, None, false, 80)?;
                Ok(())
            }
        })
        .unwrap();
    let cfg = store::load_config(&other.1).unwrap().0;
    assert_eq!(cfg.qq.app_id, APPID2);
    assert_eq!(names(&cfg.watch.channels), vec!["橘ひなの".to_string()]);
    assert_eq!(
        credentials::load_meta(&other.1).unwrap().generation,
        generation + 1
    );
    assert_eq!(load_meta(&other.1).unwrap().revision, 2);

    let watch_only = temp();
    configure(&watch_only.1);
    let appid_before = store::load_config(&watch_only.1).unwrap().0.qq.app_id;
    watch_only
        .1
        .transaction({
            let watch = watch_copy(1, "Kamito", "jp");
            move |tx| accept_tx(tx, &watch, None, 80).map(|_| ())
        })
        .unwrap();
    assert_eq!(
        store::load_config(&watch_only.1).unwrap().0.qq.app_id,
        appid_before
    );
}

#[test]
fn an_unseeded_list_rejects_a_stale_base_and_an_empty_one_still_saves() {
    let jp = temp();
    put_watch(&jp.1, true, &["Kamito", "橘ひなの"]);
    let replace = WatchSave {
        base_revision: Some(0),
        enabled: false,
        channels: Vec::new(),
        migration_notice: None,
    };
    assert_eq!(
        apply_save(&jp.1, &replace, "jp", 200),
        Err(SaveError::WatchConflict { current: 1 })
    );
    assert_eq!(
        saved(&jp.1),
        (true, vec!["Kamito".into(), "橘ひなの".into()], 1)
    );
    assert_eq!(credentials::load_meta(&jp.1).unwrap().generation, 0);
    assert!(!store::load_openid(&jp.1).unwrap().is_set());

    let panel = temp();
    put_watch(&panel.1, true, &["Kamito", "橘ひなの"]);
    let error = store::save_all_with(
        &panel.1,
        None,
        store::SecretChange::Keep,
        0,
        None,
        None,
        Some((replace.clone(), "jp".into(), 200)),
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(
        credentials::save_failed(&error),
        Some(SaveError::WatchConflict { current: 1 })
    );
    assert_eq!(
        saved(&panel.1).1,
        vec!["Kamito".to_string(), "橘ひなの".to_string()]
    );

    let holder = temp();
    put_watch(&holder.1, true, &["Kamito", "橘ひなの"]);
    let body = serde_json::to_vec(&serde_json::json!({ "watch": replace })).unwrap();
    let members = vec!["jp".to_string(), "ca".to_string()];
    assert_eq!(
        credentials::serve_forward(&holder.1, "ca", &members, &body, true, "jp", 300),
        Err(ForwardRefusal::Save(SaveError::WatchConflict {
            current: 1
        }))
    );
    assert_eq!(
        saved(&holder.1).1,
        vec!["Kamito".to_string(), "橘ひなの".to_string()]
    );
    assert_eq!(credentials::load_meta(&holder.1).unwrap().generation, 0);

    let fresh = temp();
    let saved_first = apply_save(
        &fresh.1,
        &WatchSave {
            base_revision: Some(0),
            enabled: true,
            channels: vec![WatchChannel::Configured {
                name: "Kamito".into(),
            }],
            migration_notice: None,
        },
        "ca",
        400,
    )
    .unwrap();
    assert!(saved_first.changed);
    assert_eq!(saved_first.revision, 1);
    assert_eq!(saved(&fresh.1).1, vec!["Kamito".to_string()]);
}

#[test]
fn a_pull_sends_the_list_when_the_request_advertises_watch_sync() {
    use crate::notify::openid_sync::pull_sends_watch;
    assert!(pull_sends_watch(false, Some("1")));
    assert!(pull_sends_watch(true, None));
    assert!(!pull_sends_watch(false, None));
    assert!(!pull_sends_watch(false, Some("true")));
}
