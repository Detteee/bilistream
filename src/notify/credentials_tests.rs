//! Addendum F gate tests (Clerk: a failure path for every cluster path):
//! whole-set push and pull, 409 on a stale or concurrent save, forwarding
//! to the holder (down, moved), the AppID change clearing the openid,
//! partitions, older peers, rule 7, the join pull (F1), the push-again
//! flag after a panic (F2) and redaction.

use super::*;
use crate::notify::config::NotifyConfig;
use crate::notify::observer::NodeSlot;
use crate::notify::openid_sync::{pull_all, push_all, PushCoalescer, RetryPolicy};
use crate::notify::store::{
    export_copy, load_config, load_openid, load_secret, save_settings, write_openid, OpenidChange,
    OpenidWrite, SecretChange,
};
use crate::notify::test_support::{fast_policy, MemCluster, TestStore};
use std::sync::Arc;

const APPID: &str = "102030405";
const APPID2: &str = "908070605";
const SECRET: &str = "SYNTHETICappSECRETmarker0001";
const SECRET2: &str = "SYNTHETICappSECRETmarker0002";
const OPENID: &str = "E4F0A1B2C3D4E5F60718293A4B5C6D7E";
const NOW: u64 = 1_791_702_240;

fn temp() -> TestStore {
    TestStore::new("credentials")
}

const FAST: RetryPolicy = fast_policy(4);

fn configure(store: &Store, appid: &str, secret: &str) {
    let mut cfg = NotifyConfig::default();
    cfg.qq.app_id = appid.into();
    save_settings(
        store,
        Some((&cfg, 0)),
        SecretChange::Set(Zeroizing::new(secret.into())),
        0,
        None,
    )
    .unwrap();
}

fn bind(store: &Store, writer: &str) {
    write_openid(
        store,
        OpenidWrite {
            change: OpenidChange::Set {
                value: Zeroizing::new(OPENID.into()),
                via: "manual",
            },
            expected: None,
            writer: writer.into(),
            now: NOW,
        },
    )
    .unwrap();
}

/// A holder with a seeded, bound set.
fn holder(id: &str) -> TestStore {
    let t = temp();
    configure(&t.1, APPID, SECRET);
    assert!(seed_if_needed(&t.1, id, NOW, false).unwrap());
    bind(&t.1, id);
    t
}

fn appid(store: &Store) -> String {
    load_config(store).unwrap().0.qq.app_id
}

fn secret(store: &Store) -> Option<String> {
    load_secret(store)
        .unwrap()
        .0
        .map(|s| s.expose().to_string())
}

fn save_req(base: Option<u64>, appid: Option<&str>, secret: Option<&str>) -> CredentialSave {
    CredentialSave {
        base_generation: base,
        appid: appid.map(str::to_string),
        appsecret: secret.map(str::to_string),
        clear_appsecret: false,
        watch: None,
    }
}

#[test]
fn seeding_records_the_appid_the_openid_was_bound_under() {
    let a = temp();
    assert!(
        !seed_if_needed(&a.1, "a", NOW, false).unwrap(),
        "nothing to seed"
    );
    assert!(export(&a.1).unwrap().is_none(), "never a partial set");
    configure(&a.1, APPID, SECRET);
    bind(&a.1, "a");
    assert!(seed_if_needed(&a.1, "a", NOW, false).unwrap());
    assert!(!seed_if_needed(&a.1, "a", NOW, false).unwrap(), "once");
    let meta = load_meta(&a.1).unwrap();
    assert_eq!(meta.generation, 1);
    assert_eq!(meta.openid_appid, APPID);
    assert!(!meta.openid_rebind(true, APPID));
}

#[tokio::test]
async fn the_whole_set_reaches_every_member_in_one_copy() {
    let (a, b, c) = (holder("a"), temp(), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone()), ("c", c.1.clone())]);
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert!(report.failed.is_empty());
    for store in [&b.1, &c.1] {
        assert_eq!(appid(store), APPID);
        assert_eq!(secret(store).as_deref(), Some(SECRET));
        assert!(load_openid(store).unwrap().is_set());
        assert_eq!(load_meta(store).unwrap(), load_meta(&a.1).unwrap());
    }
}

#[tokio::test]
async fn a_member_that_missed_every_push_converges_on_pull_and_ignores_lower_generations() {
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    cluster.set_down("b", u32::MAX);
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(report.failed, vec!["b".to_string()]);
    assert!(secret(&b.1).is_none());
    cluster.set_down("b", 0);
    assert!(pull_all(&b.1, &cluster.link("b")).await.unwrap());
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET));
    // a new save on a: b now holds gen N; an older copy is ignored.
    let old = export_copy(&b.1).unwrap();
    apply_save(&a.1, &save_req(None, None, Some(SECRET2)), "a", NOW + 5).unwrap();
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET2));
    let outcome = crate::notify::store::accept_copy(&b.1, &old).unwrap();
    assert!(!outcome.applied);
    assert_eq!(
        secret(&b.1).as_deref(),
        Some(SECRET2),
        "lower generation ignored"
    );
}

#[test]
fn a_stale_base_generation_gets_409_and_changes_nothing() {
    let a = holder("a");
    let gen = load_meta(&a.1).unwrap().generation;
    let first = apply_save(&a.1, &save_req(Some(gen), None, Some(SECRET2)), "a", NOW).unwrap();
    assert_eq!(first.generation, gen + 1);
    let stale = apply_save(&a.1, &save_req(Some(gen), Some(APPID2), None), "a", NOW);
    assert_eq!(stale, Err(SaveError::Conflict { current: gen + 1 }));
    assert_eq!(stale.unwrap_err().code(), "credentials_conflict");
    assert_eq!(appid(&a.1), APPID, "nothing applied");
    assert!(load_openid(&a.1).unwrap().is_set());
}

#[test]
fn concurrent_saves_on_the_holder_first_commit_wins_the_other_gets_409() {
    let a = holder("a");
    let gen = load_meta(&a.1).unwrap().generation;
    let handles: Vec<_> = [SECRET, SECRET2]
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let store = Arc::clone(&a.1);
            let value = format!("{s}x{i}");
            std::thread::spawn(move || {
                apply_save(&store, &save_req(Some(gen), None, Some(&value)), "a", NOW)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(results
        .iter()
        .any(|r| *r == Err(SaveError::Conflict { current: gen + 1 })));
    assert_eq!(load_meta(&a.1).unwrap().generation, gen + 1);
}

#[test]
fn bad_values_are_refused_before_anything_commits() {
    let a = holder("a");
    let gen = load_meta(&a.1).unwrap().generation;
    for req in [
        save_req(Some(gen), Some("12ab"), None),
        save_req(Some(gen), Some(""), None),
        save_req(Some(gen), None, Some("bad\nsecret")),
    ] {
        assert_eq!(apply_save(&a.1, &req, "a", NOW), Err(SaveError::Invalid));
    }
    assert_eq!(load_meta(&a.1).unwrap().generation, gen);
}

#[tokio::test]
async fn changing_the_appid_clears_the_openid_in_the_same_generation_everywhere() {
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert!(load_openid(&b.1).unwrap().is_set());
    let before = load_openid(&a.1).unwrap().revision;
    let gen = load_meta(&a.1).unwrap().generation;
    let outcome = apply_save(&a.1, &save_req(Some(gen), Some(APPID2), None), "a", NOW).unwrap();
    assert!(outcome.openid_cleared && outcome.changed);
    let openid = load_openid(&a.1).unwrap();
    assert!(!openid.is_set());
    assert_eq!(openid.revision, before + 1);
    let meta = load_meta(&a.1).unwrap();
    assert_eq!(
        (meta.generation, meta.openid_appid.as_str()),
        (gen + 1, APPID)
    );
    assert!(meta.openid_rebind(false, APPID2));
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(appid(&b.1), APPID2);
    assert!(!load_openid(&b.1).unwrap().is_set(), "cleared on b too");
    assert_eq!(view(&b.1).unwrap()["openid_rebind"], true);
    // The flag survives a restart and missed pushes: another member
    // learns both the cleared recipient and its rebind cause on pull.
    let c = temp();
    let pull_cluster = MemCluster::new(&[("b", b.1.clone()), ("c", c.1.clone())]);
    assert!(pull_all(&c.1, &pull_cluster.link("c")).await.unwrap());
    assert!(!load_openid(&c.1).unwrap().is_set());
    assert_eq!(view(&c.1).unwrap()["openid_rebind"], true);
    // Same AppID again is not a change and clears nothing.
    bind(&a.1, "a");
    let gen = load_meta(&a.1).unwrap().generation;
    let same = apply_save(&a.1, &save_req(Some(gen), Some(APPID2), None), "a", NOW).unwrap();
    assert!(!same.changed);
    assert!(load_openid(&a.1).unwrap().is_set());
    assert_eq!(load_meta(&a.1).unwrap().openid_appid, APPID2);
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(view(&b.1).unwrap()["openid_rebind"], false);
    assert!(load_openid(&b.1).unwrap().is_set());
}

#[tokio::test]
async fn binding_revision_order_keeps_the_winning_openids_appid_in_both_sync_orders() {
    crate::install_crypto_provider();
    const NEXT_OPENID: &str = "SYNTHETICNEWBOTOPENID000000000001";
    for other_appid in [APPID, APPID2] {
        for old_binding_first in [false, true] {
            let (a, b) = (temp(), temp());
            configure(&a.1, APPID, SECRET);
            // Pre-F history: many binds, then seed generation1. Its binding
            // revision can be higher than a newer credentials generation.
            for _ in 0..6 {
                bind(&a.1, "a");
            }
            seed_if_needed(&a.1, "a", NOW, false).unwrap();
            configure(&b.1, other_appid, SECRET);
            write_openid(
                &b.1,
                OpenidWrite {
                    change: OpenidChange::Set {
                        value: Zeroizing::new(NEXT_OPENID.into()),
                        via: "manual",
                    },
                    expected: None,
                    writer: "b".into(),
                    now: NOW,
                },
            )
            .unwrap();
            seed_if_needed(&b.1, "b", NOW + 1, false).unwrap();
            apply_save(&b.1, &save_req(Some(1), None, Some(SECRET2)), "b", NOW + 2).unwrap();
            for store in [&a.1, &b.1] {
                let (mut config, revision) = load_config(store).unwrap();
                config.enabled = true;
                crate::notify::store::save_config(store, &config, revision).unwrap();
            }
            let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
            if old_binding_first {
                push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
                pull_all(&a.1, &cluster.link("a")).await.unwrap();
            } else {
                push_all(&b.1, &cluster.link("b"), FAST).await.unwrap();
                pull_all(&b.1, &cluster.link("b")).await.unwrap();
            }
            for (id, store) in [("a", &a.1), ("b", &b.1)] {
                assert_eq!(appid(store), other_appid);
                assert_eq!(load_meta(store).unwrap().generation, 2);
                assert_eq!(load_openid(store).unwrap().openid.unwrap().expose(), OPENID);
                assert_eq!(load_meta(store).unwrap().openid_appid, APPID);
                assert_eq!(view(store).unwrap()["openid_rebind"], other_appid != APPID);
                let worker = crate::notify::worker::Worker::new(store.clone(), id.into());
                assert_eq!(
                    worker.status(NOW, &Default::default())["capable"],
                    other_appid == APPID,
                    "a binding from a different AppID must never become a send recipient"
                );
            }
            // An explicit bind on the selected bot supersedes the older
            // binding revision and restores sending everywhere.
            write_openid(
                &b.1,
                OpenidWrite {
                    change: OpenidChange::Set {
                        value: Zeroizing::new(NEXT_OPENID.into()),
                        via: "manual",
                    },
                    expected: Some(6),
                    writer: "b".into(),
                    now: NOW + 3,
                },
            )
            .unwrap();
            push_all(&b.1, &cluster.link("b"), FAST).await.unwrap();
            for (id, store) in [("a", &a.1), ("b", &b.1)] {
                assert_eq!(load_meta(store).unwrap().openid_appid, other_appid);
                assert_eq!(
                    load_openid(store).unwrap().openid.unwrap().expose(),
                    NEXT_OPENID
                );
                assert_eq!(
                    crate::notify::worker::Worker::new(store.clone(), id.into())
                        .status(NOW, &Default::default())["capable"],
                    true
                );
            }
        }
    }
}

#[test]
fn an_export_uses_one_snapshot_when_a_combined_binding_save_commits_during_projection() {
    use crate::webui::api::notify::{
        apply_update_with, credential_part, strip_credentials, UpdateNotify,
    };
    const NEXT_OPENID: &str = "SYNTHETICNEWBOTOPENID000000000001";
    let a = holder("a");
    let before = load_meta(&a.1).unwrap();
    let old_revision = load_openid(&a.1).unwrap().revision;
    let exported = crate::notify::store::export_copy_with(&a.1, || {
        // Deterministically commit between snapshot capture and projection.
        // No sleep or scheduling race is needed to exercise the old gap.
        let mut update: UpdateNotify = serde_json::from_value(serde_json::json!({
            "config": {"qq": {"app_id": APPID2}},
            "app_secret": SECRET2,
            "admin_openid": NEXT_OPENID,
            "expected_openid_revision": old_revision,
            "base_generation": before.generation,
        }))
        .unwrap();
        let save = credential_part(&a.1, &update).unwrap().unwrap();
        strip_credentials(&mut update);
        apply_update_with(
            &a.1,
            &update,
            "a",
            &|| Ok(()),
            Some((save, "a".into(), NOW)),
            |_| Ok(()),
        )
        .unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(exported.openid, OPENID);
    assert_eq!(exported.revision, old_revision);
    let credentials = exported.credentials.as_ref().unwrap();
    assert_eq!(credentials.generation, before.generation);
    assert_eq!(credentials.appid, APPID);
    assert_eq!(credentials.openid_appid, APPID);
    assert_eq!(credentials.appsecret, SECRET);
    let next = export_copy(&a.1).unwrap();
    assert_eq!(next.openid, NEXT_OPENID);
    assert_eq!(next.credentials.as_ref().unwrap().appid, APPID2);
    assert_eq!(next.credentials.as_ref().unwrap().openid_appid, APPID2);
}

#[test]
fn a_legacy_pull_gets_the_original_openid_shape_without_synced_credentials() {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyCopy {
        state: crate::notify::store::CopyState,
        openid: String,
        bound_at: Option<u64>,
        via: Option<String>,
        revision: u64,
        writer: String,
    }
    let a = holder("a");
    let members = vec!["a".into(), "b".into()];
    let copy = crate::notify::openid_sync::serve_pull(&a.1, "b", &members, false, false).unwrap();
    let bytes = serde_json::to_vec(&copy).unwrap();
    let legacy: LegacyCopy =
        serde_json::from_slice(&bytes).expect("old deny_unknown_fields decoder");
    assert_eq!(legacy.state, crate::notify::store::CopyState::Bound);
    assert_eq!(legacy.openid, OPENID);
    assert_eq!(legacy.bound_at, Some(NOW));
    assert_eq!(legacy.via.as_deref(), Some("manual"));
    assert_eq!(legacy.revision, load_openid(&a.1).unwrap().revision);
    assert_eq!(legacy.writer, "a");
    assert!(!String::from_utf8_lossy(&bytes).contains(SECRET));
    assert!(!String::from_utf8_lossy(&bytes).contains("credentials"));
    let modern = crate::notify::openid_sync::serve_pull(&a.1, "b", &members, true, true).unwrap();
    assert_eq!(modern.credentials.as_ref().unwrap().appsecret, SECRET);
}

fn binding_before_seed() -> TestStore {
    let source = temp();
    configure(&source.1, APPID, SECRET);
    bind(&source.1, "jp");
    seed_if_needed(&source.1, "ny", NOW, false).unwrap();
    source
}

fn set_binding_association(store: &Store, association: &str) {
    let mut meta = load_meta(store).unwrap();
    meta.openid_appid = association.into();
    store
        .write(CREDENTIALS_RECORD, serde_json::to_value(meta).unwrap())
        .unwrap();
}

fn document_state(store: &Store, name: &str) -> Option<serde_json::Value> {
    store
        .read(name)
        .unwrap()
        .map(|doc| serde_json::to_value(doc).unwrap())
}

#[test]
fn same_binding_full_copy_completes_a_legacy_binding_on_a_blank_receiver() {
    let source = binding_before_seed();
    let receiver = temp();
    let full = export_copy(&source.1).unwrap();
    let mut legacy = full.clone();
    legacy.credentials = None;
    let accepted = crate::notify::store::accept_copy_as(&receiver.1, &legacy, None, NOW).unwrap();
    assert!(accepted.applied);
    let binding = load_openid(&receiver.1).unwrap();
    assert_eq!(binding.revision, 1);
    assert_eq!(binding.writer, "jp");
    assert_eq!(load_meta(&receiver.1).unwrap().generation, 0);
    assert!(appid(&receiver.1).is_empty());

    let accepted = crate::notify::store::accept_copy_as(&receiver.1, &full, None, NOW + 1).unwrap();
    assert!(accepted.applied);
    assert_eq!(
        load_meta(&receiver.1).unwrap(),
        load_meta(&source.1).unwrap()
    );
    assert_eq!(
        load_openid(&receiver.1).unwrap(),
        binding,
        "binding revision and record remain untouched"
    );
    assert_eq!(appid(&receiver.1), APPID);
    assert_eq!(secret(&receiver.1).as_deref(), Some(SECRET));
    assert_eq!(view(&receiver.1).unwrap()["openid_rebind"], false);
    assert!(
        !load_config(&receiver.1).unwrap().0.enabled,
        "ca stays disabled"
    );
    let revision = receiver
        .1
        .read(CREDENTIALS_RECORD)
        .unwrap()
        .unwrap()
        .revision;
    assert!(
        !crate::notify::store::accept_copy_as(&receiver.1, &full, None, NOW + 2)
            .unwrap()
            .applied
    );
    assert_eq!(
        receiver
            .1
            .read(CREDENTIALS_RECORD)
            .unwrap()
            .unwrap()
            .revision,
        revision
    );
}

#[tokio::test]
async fn same_binding_retry_repairs_only_unknown_metadata_and_reports_a_pull_change() {
    let source = binding_before_seed();
    let receiver = temp();
    let full = export_copy(&source.1).unwrap();
    crate::notify::store::accept_copy_as(&receiver.1, &full, None, NOW).unwrap();
    // Persist the exact deployment failure: values and keys are current,
    // but the earlier legacy copy left the binding's association unknown.
    set_binding_association(&receiver.1, "");
    let before = load_meta(&receiver.1).unwrap();
    let binding = load_openid(&receiver.1).unwrap();
    let config = document_state(&receiver.1, crate::notify::store::CONFIG_RECORD);
    let secret = document_state(&receiver.1, crate::notify::store::SECRETS_RECORD);
    let revision = receiver
        .1
        .read(CREDENTIALS_RECORD)
        .unwrap()
        .unwrap()
        .revision;
    assert_eq!(view(&receiver.1).unwrap()["openid_rebind"], true);
    let cluster = MemCluster::new(&[("jp", source.1.clone()), ("ca", receiver.1.clone())]);

    assert!(
        pull_all(&receiver.1, &cluster.link("ca")).await.unwrap(),
        "metadata-only repair must report applied so the worker reloads"
    );
    let mut expected = before;
    expected.openid_appid = APPID.into();
    assert_eq!(load_meta(&receiver.1).unwrap(), expected);
    assert_eq!(load_openid(&receiver.1).unwrap(), binding);
    assert_eq!(
        document_state(&receiver.1, crate::notify::store::CONFIG_RECORD),
        config
    );
    assert_eq!(
        document_state(&receiver.1, crate::notify::store::SECRETS_RECORD),
        secret
    );
    assert_eq!(
        receiver
            .1
            .read(CREDENTIALS_RECORD)
            .unwrap()
            .unwrap()
            .revision,
        revision + 1
    );
    assert_eq!(view(&receiver.1).unwrap()["openid_rebind"], false);
    assert!(
        !pull_all(&receiver.1, &cluster.link("ca")).await.unwrap(),
        "repair is idempotent"
    );
    assert_eq!(
        receiver
            .1
            .read(CREDENTIALS_RECORD)
            .unwrap()
            .unwrap()
            .revision,
        revision + 1
    );
}

#[test]
fn same_binding_repair_never_relabels_other_bindings_or_known_associations() {
    use crate::notify::store::CopyState;
    let source = binding_before_seed();
    let full = export_copy(&source.1).unwrap();
    for case in [
        "different_openid",
        "known_association",
        "stale_writer",
        "stale_binding",
        "unbound_local",
        "unbound_copy",
        "empty_proof",
        "unusable_proof",
        "legacy_copy",
    ] {
        let receiver = temp();
        crate::notify::store::accept_copy_as(&receiver.1, &full, None, NOW).unwrap();
        let mut incoming = full.clone();
        if matches!(case, "stale_binding") {
            bind(&receiver.1, "jp");
        } else if matches!(case, "unbound_local") {
            write_openid(
                &receiver.1,
                OpenidWrite {
                    change: OpenidChange::Clear,
                    expected: Some(1),
                    writer: "jp".into(),
                    now: NOW + 1,
                },
            )
            .unwrap();
            // Equal key cannot resurrect an explicitly unbound recipient.
            incoming.revision = 2;
        }
        set_binding_association(
            &receiver.1,
            if case == "known_association" {
                APPID2
            } else {
                ""
            },
        );
        match case {
            "different_openid" => incoming.openid = "SYNTHETICOTHEROPENID000000000001".into(),
            "stale_writer" => incoming.writer = "aa".into(),
            "unbound_copy" => {
                incoming.state = CopyState::Unbound;
                incoming.openid.clear();
            }
            "empty_proof" => incoming.credentials.as_mut().unwrap().openid_appid.clear(),
            "unusable_proof" => incoming.credentials.as_mut().unwrap().appsecret.clear(),
            "legacy_copy" => incoming.credentials = None,
            _ => {}
        }
        let binding = load_openid(&receiver.1).unwrap();
        let metadata = document_state(&receiver.1, CREDENTIALS_RECORD);
        let accepted =
            crate::notify::store::accept_copy_as(&receiver.1, &incoming, None, NOW + 2).unwrap();
        assert!(!accepted.applied, "{case}");
        assert_eq!(load_openid(&receiver.1).unwrap(), binding, "{case}");
        assert_eq!(
            document_state(&receiver.1, CREDENTIALS_RECORD),
            metadata,
            "{case}"
        );
    }
}

#[test]
fn an_openid_bound_under_another_appid_is_reported_as_rebind_and_hidden() {
    let a = holder("a");
    let meta = load_meta(&a.1).unwrap();
    assert!(meta.openid_rebind(true, APPID2));
    assert!(
        meta.openid_rebind(false, APPID2),
        "retain the cleared binding's cause"
    );
    assert!(
        !CredentialsMeta::default().openid_rebind(true, APPID2),
        "legacy data stays valid"
    );
    // A peer set whose openid_appid differs from the local AppID: write the
    // AppID behind the set's back (an older node's local save).
    let mut cfg = load_config(&a.1).unwrap();
    cfg.0.qq.app_id = APPID2.into();
    crate::notify::store::save_config(&a.1, &cfg.0, cfg.1).unwrap();
    let view = view(&a.1).unwrap();
    assert_eq!(view["openid_rebind"], true);
    let settings = crate::webui::api::notify::settings_view_for_tests(&a.1).unwrap();
    assert_eq!(settings["openid_set"], false);
    assert_eq!(settings["openid_rebind"], true);
}

#[tokio::test]
async fn a_save_on_a_member_is_forwarded_to_the_holder() {
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    *cluster.holder.lock().unwrap() = Some("a".into());
    let gen = load_meta(&b.1).unwrap().generation;
    let link = cluster.link("b");
    let outcome = save(
        &b.1,
        &link,
        &Holder::Peer("a".into()),
        &save_req(Some(gen), None, Some(SECRET2)),
        "b",
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(outcome.generation, gen + 1);
    assert_eq!(
        secret(&a.1).as_deref(),
        Some(SECRET2),
        "committed on the holder"
    );
    assert_eq!(
        secret(&b.1).as_deref(),
        Some(SECRET),
        "b waits for the push"
    );
    // A stale page on b, forwarded: 409 from the holder.
    let stale = save(
        &b.1,
        &link,
        &Holder::Peer("a".into()),
        &save_req(Some(gen), Some(APPID2), None),
        "b",
        NOW,
    )
    .await;
    assert_eq!(stale, Err(SaveError::Conflict { current: gen + 1 }));
}

fn mixed_forwarded_update(store: &Store) -> crate::webui::api::notify::UpdateNotify {
    serde_json::from_value(serde_json::json!({
        "config": {"commands_enabled": false, "qq": {"app_id": APPID2}},
        "expected_revision": load_config(store).unwrap().1.unwrap(),
        "app_secret": SECRET2,
        "base_generation": load_meta(store).unwrap().generation
    }))
    .unwrap()
}
#[tokio::test]
async fn a_pre_migration_watch_body_is_refused_instead_of_migrated() {
    use crate::webui::api::notify::{credential_part, watch_part};
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    *cluster.holder.lock().unwrap() = Some("a".into());
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    b.1.write(
        "channels.json",
        serde_json::json!({"channels":[{
            "name":"canonical","aliases":[],"platforms":{"youtube":"UCwatch","twitch":"watchlogin"}
        }]}),
    )
    .unwrap();
    let mut update = mixed_forwarded_update(&b.1);
    update.base_watch_revision = Some(0);
    update.config.as_mut().unwrap()["watch"] = serde_json::json!({"enabled":true,"channels":[
        {"platform":"youtube","channel_id":"UCwatch","name":"old"},
        {"platform":"twitch","channel_id":"WATCHLOGIN","name":"old"}
    ]});
    // The credential half still extracts, but the pre-migration watch body is
    // refused instead of being folded into the account-identity model.
    assert!(credential_part(&b.1, &update).unwrap().is_some());
    let error = watch_part(&b.1, &update).unwrap_err();
    assert!(matches!(
        error,
        crate::webui::api::notify::UpdateError::Watch(_)
    ));
    let (cfg, _) = load_config(&b.1).unwrap();
    assert!(cfg.watch.channels.is_empty());
}

#[test]
fn invalid_watch_input_is_rejected_during_forward_preflight_before_credentials_commit() {
    use crate::webui::api::notify::{forwarded, strip_credentials, UpdateError};
    let b = holder("b");
    let mut update = mixed_forwarded_update(&b.1);
    update.config.as_mut().unwrap()["watch"] = serde_json::json!({"enabled":true,"channels":[
        {"kind":"configured","name":"not-configured"}
    ]});
    strip_credentials(&mut update);
    let before = snapshot(&b.1);
    let Err(error) = forwarded::prepare(&b.1, &update, "b", "a") else {
        panic!("invalid watch was accepted");
    };
    assert!(matches!(
        error,
        UpdateError::Watch(crate::notify::config::WatchError {
            code: "watch_unknown_channel",
            ..
        })
    ));
    assert_eq!(snapshot(&b.1), before);
}

#[tokio::test]
async fn a_forwarded_commit_retries_atomic_cache_failure_and_returns_the_holder_generation() {
    use crate::webui::api::notify::{credential_part, forwarded, strip_credentials};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    *cluster.holder.lock().unwrap() = Some("a".into());
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    let before = snapshot(&b.1);
    let mut update = mixed_forwarded_update(&b.1);
    let request = credential_part(&b.1, &update).unwrap().unwrap();
    strip_credentials(&mut update);
    let prepared = forwarded::prepare(&b.1, &update, "b", "a").unwrap();
    let outcome = save(
        &b.1,
        &cluster.link("b"),
        &Holder::Peer("a".into()),
        &request,
        "b",
        NOW,
    )
    .await
    .unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let cached = Arc::clone(&b.1);
    let original = before.clone();
    let complete = forwarded::complete(
        b.1.clone(),
        &cluster.link("b"),
        "a",
        outcome,
        prepared,
        move |tx| {
            // The real transaction has staged every change, but none is
            // published before commit, including on its retry.
            assert_eq!(snapshot(&cached), original);
            assert_eq!(
                tx.read(crate::notify::store::CONFIG_RECORD)?.unwrap().value["commands_enabled"],
                false
            );
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(std::io::Error::other("synthetic local cache failure"));
            }
            Ok(())
        },
    )
    .await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(complete.cache_ready);
    assert!(!complete.local_settings_pending);
    assert_eq!(complete.view["credentials_generation"], outcome.generation);
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET2));
    assert_eq!(appid(&b.1), APPID2);
    assert!(!load_openid(&b.1).unwrap().is_set());
    assert!(!load_config(&b.1).unwrap().0.commands_enabled);
    assert_eq!(
        load_meta(&a.1).unwrap().generation,
        before.4 + 1,
        "never re-forward a committed save"
    );
    let response = complete.into_response(false);
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[tokio::test]
async fn a_committed_forward_stays_successful_when_local_writes_fail_and_normal_pull_repairs_the_cache(
) {
    use crate::webui::api::notify::{credential_part, forwarded, strip_credentials};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    *cluster.holder.lock().unwrap() = Some("a".into());
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    let before = snapshot(&b.1);
    let mut update = mixed_forwarded_update(&b.1);
    let request = credential_part(&b.1, &update).unwrap().unwrap();
    strip_credentials(&mut update);
    let prepared = forwarded::prepare(&b.1, &update, "b", "a").unwrap();
    let outcome = save(
        &b.1,
        &cluster.link("b"),
        &Holder::Peer("a".into()),
        &request,
        "b",
        NOW,
    )
    .await
    .unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let complete = forwarded::complete(
        b.1.clone(),
        &cluster.link("b"),
        "a",
        outcome,
        prepared,
        move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("synthetic persistent cache failure"))
        },
    )
    .await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "bounded local retry");
    assert!(!complete.cache_ready);
    assert!(complete.local_settings_pending);
    assert_eq!(
        snapshot(&b.1),
        before,
        "no partial local credential or settings writes"
    );
    let response = complete.into_response(false);
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["credentials_generation"], outcome.generation);
    assert_eq!(body["data"]["local_settings_pending"], true);
    assert_eq!(
        body["message"],
        "机器人凭据已保存，本节点设置暂未保存，请重试。"
    );
    for value in [APPID, APPID2, SECRET, SECRET2, OPENID] {
        assert!(!String::from_utf8_lossy(&bytes).contains(value));
    }
    assert!(pull_all(&b.1, &cluster.link("b")).await.unwrap());
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET2));
    assert_eq!(appid(&b.1), APPID2);
    assert_eq!(load_meta(&b.1).unwrap().generation, outcome.generation);
    assert!(!load_openid(&b.1).unwrap().is_set());
    assert!(
        load_config(&b.1).unwrap().0.commands_enabled,
        "local preference draft remains pending"
    );
}

#[tokio::test]
async fn a_forwarded_settings_save_rebases_over_credentials_only_and_keeps_newer_local_edits() {
    use crate::webui::api::notify::{credential_part, forwarded, strip_credentials};
    for local_edit in [false, true] {
        let (a, b) = (holder("a"), temp());
        let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
        *cluster.holder.lock().unwrap() = Some("a".into());
        push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
        let mut update = mixed_forwarded_update(&b.1);
        let request = credential_part(&b.1, &update).unwrap().unwrap();
        strip_credentials(&mut update);
        let prepared = forwarded::prepare(&b.1, &update, "b", "a").unwrap();
        let outcome = save(
            &b.1,
            &cluster.link("b"),
            &Holder::Peer("a".into()),
            &request,
            "b",
            NOW,
        )
        .await
        .unwrap();
        push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
        if local_edit {
            let (mut config, revision) = load_config(&b.1).unwrap();
            config.confirm_secs += 1;
            crate::notify::store::save_config(&b.1, &config, revision).unwrap();
        }
        let complete = forwarded::complete(
            b.1.clone(),
            &cluster.link("b"),
            "a",
            outcome,
            prepared,
            |_| Ok(()),
        )
        .await;
        assert_eq!(complete.local_settings_pending, local_edit);
        assert_eq!(load_config(&b.1).unwrap().0.commands_enabled, local_edit);
        assert_eq!(appid(&b.1), APPID2);
        assert_eq!(secret(&b.1).as_deref(), Some(SECRET2));
        assert_eq!(
            complete.into_response(false).status(),
            axum::http::StatusCode::OK
        );
    }
}

#[tokio::test]
async fn a_holder_that_is_down_or_moved_applies_nothing_anywhere() {
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    let gen = load_meta(&a.1).unwrap().generation;
    let link = cluster.link("b");
    cluster.set_down("a", u32::MAX);
    *cluster.holder.lock().unwrap() = Some("a".into());
    let down = save(
        &b.1,
        &link,
        &Holder::Peer("a".into()),
        &save_req(Some(gen), Some(APPID2), None),
        "b",
        NOW,
    )
    .await;
    assert_eq!(down, Err(SaveError::HolderUnreachable));
    assert_eq!(down.unwrap_err().code(), "credentials_holder_unreachable");
    cluster.set_down("a", 0);
    *cluster.holder.lock().unwrap() = Some("c".into());
    let moved = save(
        &b.1,
        &link,
        &Holder::Peer("a".into()),
        &save_req(Some(gen), Some(APPID2), None),
        "b",
        NOW,
    )
    .await;
    assert_eq!(moved, Err(SaveError::HolderMoved));
    for store in [&a.1, &b.1] {
        assert_eq!(appid(store), APPID);
        assert_eq!(load_meta(store).unwrap().generation, gen);
        assert!(load_openid(store).unwrap().is_set());
    }
}

#[test]
fn the_holder_is_the_fresh_gateway_node_and_a_stale_one_is_no_holder() {
    let slot = |id: &str, fresh: bool, held: Option<bool>| NodeSlot {
        id: id.into(),
        fresh,
        gateway_held: held,
        ..Default::default()
    };
    assert_eq!(pick_holder("b", true, &[]), Holder::Local);
    assert_eq!(
        pick_holder(
            "b",
            false,
            &[slot("a", true, Some(true)), slot("b", true, None)]
        ),
        Holder::Peer("a".into())
    );
    assert_eq!(
        pick_holder("b", false, &[slot("a", false, Some(true))]),
        Holder::Nobody
    );
    assert_eq!(
        pick_holder("b", false, &[slot("a", true, None)]),
        Holder::Nobody
    );
}

#[tokio::test]
async fn partition_sides_that_both_saved_converge_on_the_greater_set() {
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    let gen = load_meta(&a.1).unwrap().generation;
    // Partition: no holder on either side; each saves locally, same base.
    apply_save(
        &a.1,
        &save_req(Some(gen), None, Some(SECRET2)),
        "a",
        NOW + 10,
    )
    .unwrap();
    apply_save(
        &b.1,
        &save_req(Some(gen), Some(APPID2), None),
        "b",
        NOW + 20,
    )
    .unwrap();
    // Healed: both push. (gen+1, NOW+20, "b") beats (gen+1, NOW+10, "a").
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    push_all(&b.1, &cluster.link("b"), FAST).await.unwrap();
    for store in [&a.1, &b.1] {
        assert_eq!(appid(store), APPID2);
        assert_eq!(secret(store).as_deref(), Some(SECRET));
        assert_eq!(load_meta(store).unwrap().updated_by, "b");
        assert!(
            !load_openid(store).unwrap().is_set(),
            "b's AppID change cleared it"
        );
    }
    // Same generation and time: node id breaks the tie, on both sides.
    let (x, y) = (holder("x"), temp());
    let pair = MemCluster::new(&[("x", x.1.clone()), ("y", y.1.clone())]);
    push_all(&x.1, &pair.link("x"), FAST).await.unwrap();
    let g = load_meta(&x.1).unwrap().generation;
    apply_save(&x.1, &save_req(Some(g), None, Some(SECRET2)), "x", NOW + 30).unwrap();
    apply_save(
        &y.1,
        &save_req(Some(g), None, Some("SYNTHETICotherSIDE")),
        "y",
        NOW + 30,
    )
    .unwrap();
    push_all(&x.1, &pair.link("x"), FAST).await.unwrap();
    push_all(&y.1, &pair.link("y"), FAST).await.unwrap();
    assert_eq!(secret(&x.1), secret(&y.1));
    assert_eq!(secret(&x.1).as_deref(), Some("SYNTHETICotherSIDE"));
}

#[tokio::test]
async fn an_empty_appid_or_secret_from_a_peer_is_never_a_change() {
    let (a, b) = (holder("a"), holder("b"));
    let mut copy = export_copy(&a.1).unwrap();
    let set = copy.credentials.as_mut().unwrap();
    set.generation = 99;
    set.appsecret = String::new();
    let out = crate::notify::store::accept_copy(&b.1, &copy).unwrap();
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET));
    assert!(load_meta(&b.1).unwrap().generation < 99);
    let _ = out;
    let set = copy.credentials.as_mut().unwrap();
    set.appsecret = SECRET2.into();
    set.appid = String::new();
    crate::notify::store::accept_copy(&b.1, &copy).unwrap();
    assert_eq!(appid(&b.1), APPID);
}

#[tokio::test]
async fn an_older_peer_gets_the_openid_without_the_set() {
    let (a, old) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("old", old.1.clone())]);
    cluster.legacy.lock().unwrap().insert("old".into());
    let report = push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert!(report.failed.is_empty(), "acked without the set");
    assert!(load_openid(&old.1).unwrap().is_set());
    assert!(secret(&old.1).is_none(), "never pushed to an older node");
    assert_eq!(load_meta(&old.1).unwrap().generation, 0);
}

#[test]
fn nothing_secret_appears_in_debug_json_views_or_error_text() {
    let a = holder("a");
    let copy = export_copy(&a.1).unwrap();
    let req = save_req(Some(1), Some(APPID2), Some(SECRET2));
    let mut text = format!("{copy:?} {req:?} {:?}", copy.credentials);
    text.push_str(&view(&a.1).unwrap().to_string());
    text.push_str(
        &crate::webui::api::notify::settings_view_for_tests(&a.1)
            .unwrap()
            .to_string(),
    );
    for error in [
        SaveError::Conflict { current: 3 },
        SaveError::Invalid,
        SaveError::HolderUnreachable,
        SaveError::HolderMoved,
        SaveError::Store,
    ] {
        text.push_str(error.message());
        text.push_str(error.code());
        text.push_str(&format!("{error:?}"));
    }
    text.push_str(&format!("{:?}", load_meta(&a.1).unwrap()));
    for secret in [SECRET, SECRET2, OPENID, &SECRET[..12], &OPENID[..8]] {
        assert!(!text.contains(secret), "leaked a fragment");
    }
    // Control: the copy itself does carry them on the signed route.
    let wire = serde_json::to_string(&copy).unwrap();
    assert!(wire.contains(SECRET) && wire.contains(OPENID));
}

#[test]
fn the_set_record_stays_out_of_backups() {
    assert!(!crate::storage::NAMES.contains(&CREDENTIALS_RECORD));
}

#[test]
fn a_forwarded_reply_maps_to_fixed_codes() {
    use crate::notify::openid_sync::forward_reply;
    let ok = br#"{"success":true,"data":{"generation":4,"changed":true,"openid_cleared":false}}"#;
    assert_eq!(forward_reply(200, ok).unwrap().generation, 4);
    let conflict =
        br#"{"success":false,"data":{"code":"credentials_conflict","current_generation":7}}"#;
    assert_eq!(
        forward_reply(409, conflict),
        Err(SaveError::Conflict { current: 7 })
    );
    let moved = br#"{"success":false,"data":{"code":"credentials_holder_moved"}}"#;
    assert_eq!(forward_reply(409, moved), Err(SaveError::HolderMoved));
    assert_eq!(forward_reply(400, b"{}"), Err(SaveError::Invalid));
    for status in [403, 500, 502, 503] {
        assert_eq!(
            forward_reply(status, b"x"),
            Err(SaveError::HolderUnreachable)
        );
    }
    assert_eq!(
        forward_reply(200, b"not json"),
        Err(SaveError::HolderUnreachable)
    );
}

#[test]
fn a_forward_from_a_non_member_or_with_a_bad_body_is_refused() {
    let a = holder("a");
    let body = serde_json::to_vec(&save_req(Some(1), Some(APPID2), None)).unwrap();
    let members = vec!["a".to_string(), "b".to_string()];
    assert_eq!(
        serve_forward(&a.1, "z", &members, &body, true, "a", NOW),
        Err(ForwardRefusal::NotMember)
    );
    assert_eq!(
        serve_forward(&a.1, "b", &members, b"{\"appsecret\":1}", true, "a", NOW),
        Err(ForwardRefusal::Save(SaveError::Invalid))
    );
    assert_eq!(appid(&a.1), APPID);
}

#[tokio::test]
async fn the_peer_save_route_refuses_unsigned_and_plain_http_callers() {
    let response = crate::webui::api::notify::peer_credentials_save(
        None,
        axum::http::HeaderMap::new(),
        axum::body::Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-forwarded-proto", "http".parse().unwrap());
    let response = crate::webui::api::notify::peer_credentials_save(
        Some(axum::Extension(
            crate::cluster::peer_call::AuthenticatedNode {
                member_id: "m".into(),
                node_id: "a".into(),
            },
        )),
        headers,
        axum::body::Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

#[test]
fn missing_codes_tell_pending_from_unset_and_report_rebind() {
    use crate::notify::readiness::{missing_synced, ClusterHas, SyncState};
    let cluster = ClusterHas {
        generation: 3,
        appid: true,
        appsecret: true,
    };
    let behind = SyncState {
        generation: Some(0),
        rebind: false,
    };
    assert_eq!(
        missing_synced(true, (false, false, false), Some(true), behind, cluster),
        vec!["appid_pending", "appsecret_pending", "openid_pending"]
    );
    // Behind but with old values: still pending (never the sender).
    assert_eq!(
        missing_synced(true, (true, true, true), Some(true), behind, cluster),
        vec!["appid_pending", "appsecret_pending"]
    );
    let current = SyncState {
        generation: Some(3),
        rebind: true,
    };
    assert_eq!(
        missing_synced(true, (true, true, false), Some(true), current, cluster),
        vec!["openid_rebind"]
    );
    let none = ClusterHas {
        generation: 3,
        appid: false,
        appsecret: false,
    };
    assert_eq!(
        missing_synced(
            true,
            (false, false, false),
            None,
            SyncState {
                generation: Some(3),
                rebind: false
            },
            none
        ),
        vec!["appid", "appsecret", "openid"]
    );
    assert_eq!(
        missing_synced(false, (true, true, true), None, current, cluster),
        vec!["stale"]
    );
}

/// F1 (Clerk, 1b gate): at worker level, joining at a new membership
/// revision really starts a pull, and that pull brings the whole set.
#[tokio::test]
async fn a_joining_worker_starts_a_pull_that_brings_the_whole_set() {
    crate::install_crypto_provider();
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone())]);
    let mut worker = crate::notify::worker::Worker::new(Arc::clone(&b.1), "b".into());
    cluster.add("b", Arc::clone(&b.1));
    let revision = *cluster.revision.lock().unwrap();
    let mut started: Option<tokio::task::JoinHandle<bool>> = None;
    worker.maybe_pull_at(NOW, revision, || {
        let (store, link) = (Arc::clone(&b.1), cluster.link("b"));
        started = Some(tokio::spawn(async move {
            pull_all(&store, &link).await.unwrap()
        }));
    });
    let pulled = started.expect("joining starts a pull").await.unwrap();
    assert!(pulled);
    assert_eq!(appid(&b.1), APPID);
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET));
    assert!(load_openid(&b.1).unwrap().is_set());
    // Same revision a minute later: no second pull; a new revision: pull.
    let mut again = false;
    worker.maybe_pull_at(NOW + 60, revision, || again = true);
    assert!(!again);
    worker.maybe_pull_at(NOW + 61, revision + 1, || again = true);
    assert!(again);
}

/// F2 (Clerk, 1b gate): a push request that arrives while a round panics
/// keeps its "push again" flag, and a new round starts and delivers it.
#[tokio::test]
async fn a_push_requested_during_a_panicking_round_still_goes_out() {
    let (a, b) = (holder("a"), temp());
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    let coalescer: &'static PushCoalescer = Box::leak(Box::new(PushCoalescer::new()));
    assert!(coalescer.request());
    let rounds = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&rounds);
    let (store, link) = (Arc::clone(&a.1), cluster.link("a"));
    let link = Arc::new(link);
    coalescer
        .drive(move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (store, link) = (Arc::clone(&store), Arc::clone(&link));
            async move {
                if n == 0 {
                    // A save lands mid-round, then the round crashes.
                    assert!(!coalescer.request(), "merged into push again");
                    panic!("synthetic round failure");
                }
                crate::notify::openid_sync::push_round(&store, link.as_ref(), FAST).await;
            }
        })
        .await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET));
    assert!(coalescer.request(), "idle again afterwards");
}

fn snapshot(store: &Store) -> (String, Option<String>, bool, u64, u64, Option<u64>) {
    (
        appid(store),
        secret(store),
        load_openid(store).unwrap().is_set(),
        load_openid(store).unwrap().revision,
        load_meta(store).unwrap().generation,
        load_config(store).unwrap().1,
    )
}

fn settings_update(expected_revision: Option<u64>) -> crate::webui::api::notify::UpdateNotify {
    crate::webui::api::notify::UpdateNotify {
        config: Some(serde_json::json!({"live_events_enabled": false})),
        expected_revision,
        ..Default::default()
    }
}

/// Rev 2 item 15 / Clerk (b): a save that fails partway through its
/// transaction (after the AppID change, the openid clearing and the
/// generation bump were written) leaves everything exactly as before.
#[test]
fn a_save_that_fails_partway_leaves_appid_secret_openid_and_generation_untouched() {
    use crate::webui::api::notify::apply_update_with;
    let a = holder("a");
    let before = snapshot(&a.1);
    let gen = before.4;
    let mut req = save_req(Some(gen), Some(APPID2), Some(SECRET2));
    req.clear_appsecret = false;
    let result = apply_update_with(
        &a.1,
        &settings_update(before.5),
        "a",
        &|| Ok(()),
        Some((req.clone(), "a".into(), NOW)),
        |_| Err(std::io::Error::other("synthetic failure after every write")),
    );
    assert!(result.is_err());
    assert_eq!(snapshot(&a.1), before, "nothing landed");
    // The rest of the settings refused (stale config revision): the
    // credential part is not applied either.
    let stale = apply_update_with(
        &a.1,
        &settings_update(Some(before.5.unwrap_or(0) + 7)),
        "a",
        &|| Ok(()),
        Some((req.clone(), "a".into(), NOW)),
        |_| Ok(()),
    );
    assert!(stale.is_err());
    assert_eq!(snapshot(&a.1), before);
    // A stale base generation: the rest of the settings is not saved.
    let conflict = apply_update_with(
        &a.1,
        &settings_update(before.5),
        "a",
        &|| Ok(()),
        Some((save_req(Some(gen + 3), Some(APPID2), None), "a".into(), NOW)),
        |_| Ok(()),
    );
    assert!(matches!(
        conflict,
        Err(crate::webui::api::notify::UpdateError::Credentials(
            SaveError::Conflict { .. }
        ))
    ));
    assert_eq!(snapshot(&a.1), before);
    assert!(load_config(&a.1).unwrap().0.live_events_enabled);
    // Control: the same save without the failure commits all of it at once.
    let view = apply_update_with(
        &a.1,
        &settings_update(before.5),
        "a",
        &|| Ok(()),
        Some((req, "a".into(), NOW)),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(appid(&a.1), APPID2);
    assert_eq!(secret(&a.1).as_deref(), Some(SECRET2));
    assert!(!load_openid(&a.1).unwrap().is_set());
    assert_eq!(
        load_meta(&a.1).unwrap().generation,
        gen + 1,
        "one generation"
    );
    assert!(!load_config(&a.1).unwrap().0.live_events_enabled);
    assert!(!view.to_string().contains(SECRET2));
}

/// Rev 2 item 11 / Clerk (c): the AppSecret is sealed at rest like the
/// API keys; no file under the store directory holds it in plain text.
#[test]
fn the_store_files_never_hold_the_appsecret_in_plain_text() {
    let a = holder("a");
    apply_save(&a.1, &save_req(None, None, Some(SECRET2)), "a", NOW).unwrap();
    // Control: it is readable through the store.
    assert_eq!(secret(&a.1).as_deref(), Some(SECRET2));
    fn walk(dir: &std::path::Path, out: &mut Vec<Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push(bytes);
            }
        }
    }
    let mut files = Vec::new();
    walk(&a.0, &mut files);
    assert!(!files.is_empty());
    for bytes in files {
        for needle in [SECRET, SECRET2, OPENID] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "plain text on disk"
            );
        }
    }
}

fn seeded(id: &str, appid_: &str, secret_: &str, at: u64, holder: bool) -> TestStore {
    let t = temp();
    configure(&t.1, appid_, secret_);
    assert!(seed_if_needed(&t.1, id, at, holder).unwrap());
    t
}

/// Rev 2 item 12: when seeds meet and a gateway holder exists, its values
/// win, whatever the clock says; the holder re-stamps them and pushes.
#[tokio::test]
async fn migration_with_a_holder_keeps_the_holders_values() {
    // a was bound before the upgrade, so its seed carries the openid.
    let a = temp();
    configure(&a.1, APPID, SECRET);
    bind(&a.1, "a");
    assert!(seed_if_needed(&a.1, "a", NOW, false).unwrap());
    assert_eq!(load_meta(&a.1).unwrap().generation, 1);
    // b seeded later, so b's (generation, time, node) is greater.
    let b = seeded("b", APPID2, SECRET2, NOW + 100, false);
    let from_b = export_copy(&b.1).unwrap();
    let outcome =
        crate::notify::store::accept_copy_as(&a.1, &from_b, Some("a"), NOW + 200).unwrap();
    assert!(outcome.restamped && !outcome.applied);
    assert_eq!(appid(&a.1), APPID, "holder keeps its values");
    assert_eq!(load_meta(&a.1).unwrap().generation, 2);
    let cluster = MemCluster::new(&[("a", a.1.clone()), ("b", b.1.clone())]);
    push_all(&a.1, &cluster.link("a"), FAST).await.unwrap();
    assert_eq!(appid(&b.1), APPID);
    assert_eq!(secret(&b.1).as_deref(), Some(SECRET));
    let notice = load_meta(&b.1).unwrap().migration_notice.unwrap();
    assert_eq!(notice.differed, vec!["openid_set".to_string()]);
    assert_eq!(notice.winner, "a");
    let holder_notice = load_meta(&a.1).unwrap().migration_notice.unwrap();
    assert_eq!(holder_notice.differed, vec!["openid_set".to_string()]);
}

/// A seed made while holding the gateway beats a later non-holder seed.
#[test]
fn a_holder_seed_beats_a_later_seed() {
    let a = seeded("a", APPID, SECRET, NOW, true);
    let b = seeded("b", APPID2, SECRET2, NOW + 100, false);
    let from_a = export_copy(&a.1).unwrap();
    let outcome = crate::notify::store::accept_copy_as(&b.1, &from_a, None, NOW).unwrap();
    assert!(outcome.applied);
    assert_eq!(appid(&b.1), APPID);
    let from_b = export_copy(&b.1).unwrap();
    assert!(
        !crate::notify::store::accept_copy_as(&a.1, &from_b, None, NOW)
            .unwrap()
            .applied
    );
}

/// Without a holder, ties fall back to (generation, updated_at, node_id),
/// and the losing side records which flags differed, never the values.
#[test]
fn migration_without_a_holder_uses_the_tie_order_and_records_flags_only() {
    let a = seeded("a", APPID, SECRET, NOW, false);
    let b = temp();
    let mut cfg = NotifyConfig::default();
    cfg.qq.app_id = APPID2.into();
    crate::notify::store::save_config(&b.1, &cfg, None).unwrap();
    // b has only an AppID; a has both and is newer.
    let from_a = export_copy(&a.1).unwrap();
    assert!(
        crate::notify::store::accept_copy_as(&b.1, &from_a, None, NOW)
            .unwrap()
            .applied
    );
    let view = view(&b.1).unwrap();
    assert_eq!(
        view["migration_notice"]["differed"],
        serde_json::json!(["appsecret_set"])
    );
    assert_eq!(view["migration_notice"]["winner"], "a");
    let text = view.to_string();
    for value in [APPID, APPID2, SECRET] {
        assert!(!text.contains(value));
    }
    // The newer seed wins on the other side too.
    let older = seeded("c", APPID2, SECRET2, NOW - 100, false);
    let from_c = export_copy(&older.1).unwrap();
    assert!(
        !crate::notify::store::accept_copy_as(&a.1, &from_c, None, NOW)
            .unwrap()
            .applied
    );
    assert_eq!(appid(&a.1), APPID);
}

/// F rule 16: the AppID is write-only. GET carries `qq.appid_set` and
/// never `qq.app_id`; no view, status or debug output holds the value.
#[test]
fn the_appid_never_appears_in_a_response_status_or_debug_output() {
    let a = holder("a");
    let settings = crate::webui::api::notify::settings_view_for_tests(&a.1).unwrap();
    assert!(settings["config"]["qq"].get("app_id").is_none());
    assert_eq!(settings["config"]["qq"]["appid_set"], true);
    assert_eq!(settings["config"]["qq"]["app_id_hint"], "0405");
    let mut text = settings.to_string().replace("\"0405\"", "");
    text.push_str(&view(&a.1).unwrap().to_string());
    let mut worker = crate::notify::worker::Worker::new(Arc::clone(&a.1), "a".into());
    worker.reload_settings();
    let digest = crate::notify::observer::ClusterDigest {
        local: "a".into(),
        ..Default::default()
    };
    // The worker status feeds /api/notify/status, /status and the SSE
    // "status" event.
    text.push_str(&worker.status(NOW, &digest).to_string());
    text.push_str(&format!("{:?}", export_copy(&a.1).unwrap()));
    let target = crate::notify::qq::QqTarget {
        app_id: APPID.into(),
        secret: crate::notify::store::AppSecret::new(SECRET),
        openid: OPENID.into(),
        api_base: String::new(),
        token_url: String::new(),
    };
    text.push_str(&format!("{target:?}"));
    for value in [APPID, SECRET, OPENID] {
        assert!(!text.contains(value), "value leaked");
    }
    // Empty settings: the flag is false, still no key.
    let b = temp();
    let empty = crate::webui::api::notify::settings_view_for_tests(&b.1).unwrap();
    assert_eq!(empty["config"]["qq"]["appid_set"], false);
    assert!(empty["config"]["qq"]["app_id_hint"].is_null());
    assert!(empty["config"]["qq"].get("app_id").is_none());
}

/// F rule 16 on PUT: absent or null = unchanged, an explicit empty value
/// is 400, a new value is a change.
#[test]
fn a_put_without_an_appid_keeps_it_and_an_empty_one_is_refused() {
    use crate::webui::api::notify::{apply_update, credential_part, UpdateNotify};
    let a = holder("a");
    let put = |qq: serde_json::Value| UpdateNotify {
        config: Some(serde_json::json!({ "qq": qq })),
        ..Default::default()
    };
    assert!(credential_part(&a.1, &put(serde_json::json!({})))
        .unwrap()
        .is_none());
    assert!(
        credential_part(&a.1, &put(serde_json::json!({"app_id": null})))
            .unwrap()
            .is_none()
    );
    assert!(
        credential_part(&a.1, &put(serde_json::json!({"app_id": APPID})))
            .unwrap()
            .is_none(),
        "same value is no change"
    );
    for empty in ["", "   "] {
        let part = credential_part(&a.1, &put(serde_json::json!({"app_id": empty})))
            .unwrap()
            .expect("explicit empty is a change request");
        assert_eq!(validate_save(&part), Err(SaveError::Invalid));
    }
    let part = credential_part(&a.1, &put(serde_json::json!({"app_id": APPID2})))
        .unwrap()
        .unwrap();
    assert_eq!(part.appid.as_deref(), Some(APPID2));
    // A rest-of-settings save that echoes `appid_set` and `app_id: null`
    // changes neither the AppID nor fails.
    let rev = load_config(&a.1).unwrap().1;
    apply_update(
        &a.1,
        &UpdateNotify {
            config: Some(serde_json::json!({
                "qq": {"app_id": null, "appid_set": true},
                "live_events_enabled": false
            })),
            expected_revision: rev,
            ..Default::default()
        },
        "a",
        &|| Ok(()),
    )
    .unwrap();
    assert_eq!(appid(&a.1), APPID);
    assert!(!load_config(&a.1).unwrap().0.live_events_enabled);
}

/// F rev 3 rule 16: `app_id_hint` is the last 4 characters, null when no
/// AppID is set, and null for an AppID of 4 characters or fewer (the hint
/// would otherwise be the whole value).
#[test]
fn the_appid_hint_is_the_last_four_characters_or_null() {
    use crate::webui::api::notify::app_id_hint;
    assert_eq!(app_id_hint(APPID).as_deref(), Some("0405"));
    assert_eq!(app_id_hint(" 1234567890 ").as_deref(), Some("7890"));
    assert_eq!(app_id_hint("12345").as_deref(), Some("2345"));
    assert_eq!(app_id_hint(""), None);
    assert_eq!(app_id_hint("   "), None);
    assert_eq!(app_id_hint("123"), None, "shorter than 4");
    assert_eq!(app_id_hint("1234"), None, "exactly 4 would be the whole");
    // A legacy short AppID in the store: the view shows set, hint null,
    // and never the value.
    let a = temp();
    let mut cfg = NotifyConfig::default();
    cfg.qq.app_id = "987".into();
    let value = serde_json::to_value(&cfg).unwrap();
    a.1.write(crate::notify::store::CONFIG_RECORD, value)
        .unwrap();
    let view = crate::webui::api::notify::settings_view_for_tests(&a.1).unwrap();
    assert_eq!(view["config"]["qq"]["appid_set"], true);
    assert!(view["config"]["qq"]["app_id_hint"].is_null());
    assert!(!view.to_string().contains("987"));
}

/// The panel's PUT body: `app_secret` top level next to `config.qq` and
/// `base_generation`. It goes the handler's way (credential_part, strip,
/// apply_update_with) and lands in ONE transaction and ONE generation with
/// the AppID, the rest of the settings and an openid in the same PUT; a
/// failure leaves all of it unchanged.
#[test]
fn the_top_level_app_secret_commits_with_the_appid_and_settings_in_one_generation() {
    use crate::webui::api::notify::{
        apply_update_with, credential_part, strip_credentials, UpdateNotify,
    };
    let a = holder("a");
    let before = snapshot(&a.1);
    let body = |rev: Option<u64>, gen: u64| -> UpdateNotify {
        serde_json::from_value(serde_json::json!({
            "config": {"qq": {"app_id": APPID2}, "live_events_enabled": false},
            "expected_revision": rev,
            "app_secret": SECRET2,
            "base_generation": gen,
        }))
        .unwrap()
    };
    let run = |mut update: UpdateNotify, fail: bool| {
        let save = credential_part(&a.1, &update).unwrap().expect("a change");
        assert_eq!(save.appsecret.as_deref(), Some(SECRET2));
        strip_credentials(&mut update);
        apply_update_with(
            &a.1,
            &update,
            "a",
            &|| Ok(()),
            Some((save, "a".into(), NOW)),
            move |_| {
                if fail {
                    Err(std::io::Error::other("synthetic"))
                } else {
                    Ok(())
                }
            },
        )
    };
    assert!(run(body(before.5, before.4), true).is_err());
    assert_eq!(
        snapshot(&a.1),
        before,
        "secret, AppID, settings all unchanged"
    );
    run(body(before.5, before.4), false).unwrap();
    assert_eq!(appid(&a.1), APPID2);
    assert_eq!(secret(&a.1).as_deref(), Some(SECRET2));
    assert!(!load_config(&a.1).unwrap().0.live_events_enabled);
    assert_eq!(
        load_meta(&a.1).unwrap().generation,
        before.4 + 1,
        "one generation"
    );
}

/// A PUT with only the top-level `app_secret` (no `config`, no `config.qq`)
/// changes the AppSecret alone, bumps the generation once and leaves the
/// AppID and the settings revision as they were.
#[test]
fn a_put_with_only_app_secret_and_no_config_qq_still_saves_it() {
    use crate::webui::api::notify::{
        apply_update_with, credential_part, strip_credentials, UpdateNotify,
    };
    let a = holder("a");
    let before = snapshot(&a.1);
    for body in [
        serde_json::json!({"app_secret": SECRET2, "base_generation": before.4}),
        serde_json::json!({"config": {}, "app_secret": SECRET, "base_generation": before.4 + 1}),
    ] {
        let mut update: UpdateNotify = serde_json::from_value(body).unwrap();
        let save = credential_part(&a.1, &update).unwrap().expect("a change");
        assert!(save.appid.is_none());
        strip_credentials(&mut update);
        apply_update_with(
            &a.1,
            &update,
            "a",
            &|| Ok(()),
            Some((save, "a".into(), NOW)),
            |_| Ok(()),
        )
        .unwrap();
    }
    assert_eq!(appid(&a.1), APPID);
    assert_eq!(secret(&a.1).as_deref(), Some(SECRET));
    assert_eq!(load_meta(&a.1).unwrap().generation, before.4 + 2);
    assert_eq!(load_config(&a.1).unwrap().1, before.5, "settings untouched");
    assert!(
        load_openid(&a.1).unwrap().is_set(),
        "same AppID keeps the openid"
    );
}

/// strip_credentials keeps an openid sent in the same PUT.
#[test]
fn stripping_the_credentials_keeps_the_openid_in_the_same_put() {
    use crate::webui::api::notify::{strip_credentials, UpdateNotify};
    let mut update: UpdateNotify = serde_json::from_value(serde_json::json!({
        "app_secret": SECRET2, "admin_openid": OPENID, "expected_openid_revision": 1
    }))
    .unwrap();
    strip_credentials(&mut update);
    assert!(update.app_secret.is_none());
    assert_eq!(update.admin_openid.as_deref(), Some(OPENID));
}

#[test]
fn a_manual_binding_and_credentials_in_one_put_commit_under_one_generation() {
    use crate::webui::api::notify::{
        apply_update_with, credential_part, strip_credentials, UpdateNotify,
    };
    const NEXT_OPENID: &str = "SYNTHETICREBOUNDOPENID000000000001";
    for change_appid in [false, true] {
        let a = holder("a");
        let before = snapshot(&a.1);
        let previous_openid_revision = load_openid(&a.1).unwrap().revision;
        let mut update: UpdateNotify = serde_json::from_value(serde_json::json!({
            "config": {"commands_enabled": false, "qq": {"app_id": if change_appid { APPID2 } else { APPID }}},
            "expected_revision": load_config(&a.1).unwrap().1,
            "app_secret": SECRET2,
            "admin_openid": NEXT_OPENID,
            "expected_openid_revision": previous_openid_revision,
            "base_generation": before.4
        })).unwrap();
        let save = credential_part(&a.1, &update).unwrap().unwrap();
        strip_credentials(&mut update);
        // Failure after the combined binding and credential writes leaves
        // both logical revisions and every value exactly as before.
        assert!(apply_update_with(
            &a.1,
            &update,
            "a",
            &|| Ok(()),
            Some((save.clone(), "a".into(), NOW)),
            |_| { Err(std::io::Error::other("synthetic combined bind failure")) }
        )
        .is_err());
        assert_eq!(snapshot(&a.1), before);
        apply_update_with(
            &a.1,
            &update,
            "a",
            &|| Ok(()),
            Some((save, "a".into(), NOW)),
            |_| Ok(()),
        )
        .unwrap();
        let state = load_openid(&a.1).unwrap();
        assert_eq!(state.openid.unwrap().expose(), NEXT_OPENID);
        assert_eq!(state.revision, previous_openid_revision + 1);
        assert_eq!(load_meta(&a.1).unwrap().generation, before.4 + 1);
        assert_eq!(
            load_meta(&a.1).unwrap().openid_appid,
            if change_appid { APPID2 } else { APPID }
        );
        assert_eq!(secret(&a.1).as_deref(), Some(SECRET2));
        assert_eq!(view(&a.1).unwrap()["openid_rebind"], false);
        assert!(!load_config(&a.1).unwrap().0.commands_enabled);
    }
}

/// (a) An app_secret-only PUT with a stale top-level `base_generation` is
/// 409 `credentials_conflict` and changes nothing.
#[test]
fn an_app_secret_only_put_with_a_stale_base_generation_gets_409() {
    use crate::webui::api::notify::{
        apply_update_with, credential_part, credentials_status, strip_credentials, UpdateError,
        UpdateNotify,
    };
    let a = holder("a");
    let before = snapshot(&a.1);
    let mut update: UpdateNotify = serde_json::from_value(
        serde_json::json!({"app_secret": SECRET2, "base_generation": before.4 + 5}),
    )
    .unwrap();
    let save = credential_part(&a.1, &update).unwrap().unwrap();
    strip_credentials(&mut update);
    let result = apply_update_with(
        &a.1,
        &update,
        "a",
        &|| Ok(()),
        Some((save, "a".into(), NOW)),
        |_| Ok(()),
    );
    let Err(UpdateError::Credentials(error)) = result else {
        panic!("expected a conflict")
    };
    assert!(matches!(error, SaveError::Conflict { .. }));
    assert_eq!(credentials_status(error), axum::http::StatusCode::CONFLICT);
    assert_eq!(snapshot(&a.1), before);
}

/// (b) The generation check reads the top-level `base_generation`; a
/// `config.qq.base_generation` is not it (stale or current there changes
/// nothing about the outcome).
#[test]
fn the_generation_check_reads_the_top_level_base_generation_only() {
    use crate::webui::api::notify::{credential_part, UpdateNotify};
    let a = holder("a");
    let gen = load_meta(&a.1).unwrap().generation;
    let parse = |v: serde_json::Value| -> UpdateNotify { serde_json::from_value(v).unwrap() };
    // Only inside config.qq: no base generation is taken from there.
    let nested = parse(serde_json::json!({
        "config": {"qq": {"base_generation": gen + 9}}, "app_secret": SECRET2
    }));
    assert_eq!(
        credential_part(&a.1, &nested)
            .unwrap()
            .unwrap()
            .base_generation,
        None
    );
    // Top-level current + stale nested: the top level decides (accepted).
    let both = parse(serde_json::json!({
        "config": {"qq": {"base_generation": gen + 9}},
        "app_secret": SECRET2, "base_generation": gen
    }));
    let save = credential_part(&a.1, &both).unwrap().unwrap();
    assert_eq!(save.base_generation, Some(gen));
    apply_save(&a.1, &save, "a", NOW).unwrap();
    assert_eq!(secret(&a.1).as_deref(), Some(SECRET2));
    // Top-level stale + current nested: 409.
    let stale = parse(serde_json::json!({
        "config": {"qq": {"base_generation": gen + 1}},
        "app_secret": SECRET, "base_generation": gen
    }));
    let save = credential_part(&a.1, &stale).unwrap().unwrap();
    assert!(matches!(
        apply_save(&a.1, &save, "a", NOW),
        Err(SaveError::Conflict { .. })
    ));
}

/// Rule 1 is on: the switch and its function.
#[test]
fn base_generation_required_is_on_and_is_a_separate_switch() {
    let save = save_req(None, None, Some(SECRET2));
    const { assert!(REQUIRE_BASE_GENERATION) };
    assert!(base_generation_missing(&save, REQUIRE_BASE_GENERATION));
    assert!(base_generation_missing(&save, true));
    assert!(!base_generation_missing(
        &save_req(Some(1), None, Some(SECRET2)),
        true
    ));
}

/// 清除 AppSecret: the clear PUT (clear_app_secret, expected_secret_revision
/// and a top-level base_generation) is checked like a save: stale is 409 and
/// changes nothing; missing is 400 (rule 1, same function);
/// current clears, bumps the generation once and syncs to the peer.
#[test]
fn clearing_the_app_secret_checks_bumps_and_syncs_the_generation() {
    use crate::webui::api::notify::{
        apply_update_with, credential_part, strip_credentials, UpdateError, UpdateNotify,
    };
    let a = holder("a");
    let before = snapshot(&a.1);
    let secret_rev = load_secret(&a.1).unwrap().1;
    let clear = |base: Option<u64>| -> UpdateNotify {
        serde_json::from_value(serde_json::json!({
            "clear_app_secret": true,
            "expected_secret_revision": secret_rev,
            "base_generation": base,
        }))
        .unwrap()
    };
    let run = |mut update: UpdateNotify| {
        let save = credential_part(&a.1, &update)
            .unwrap()
            .expect("a clear is a change");
        assert!(save.clear_appsecret);
        let missing = base_generation_missing(&save, REQUIRE_BASE_GENERATION);
        strip_credentials(&mut update);
        (
            missing,
            apply_update_with(
                &a.1,
                &update,
                "a",
                &|| Ok(()),
                Some((save, "a".into(), NOW)),
                |_| Ok(()),
            ),
        )
    };
    let (_, stale) = run(clear(Some(before.4 + 4)));
    assert!(matches!(
        stale,
        Err(UpdateError::Credentials(SaveError::Conflict { .. }))
    ));
    assert_eq!(snapshot(&a.1), before, "stale clear changes nothing");
    let save = credential_part(&a.1, &clear(None)).unwrap().unwrap();
    assert!(
        base_generation_missing(&save, true),
        "the switch covers the clear"
    );
    let (missing, ok) = run(clear(Some(before.4)));
    assert!(!missing);
    ok.unwrap();
    assert_eq!(secret(&a.1), None);
    assert_eq!(load_meta(&a.1).unwrap().generation, before.4 + 1);
    // Sync: a peer accepting the export takes the clear and the generation.
    let b = temp();
    crate::notify::store::accept_copy_as(&b.1, &export_copy(&a.1).unwrap(), None, NOW).unwrap();
    assert_eq!(secret(&b.1), None);
    assert_eq!(load_meta(&b.1).unwrap().generation, before.4 + 1);
    // Rule 1 on: a clear without base_generation is refused (400).
    let a2 = holder("a");
    let rev2 = load_secret(&a2.1).unwrap().1;
    let update: UpdateNotify = serde_json::from_value(serde_json::json!({
        "clear_app_secret": true, "expected_secret_revision": rev2
    }))
    .unwrap();
    let save = credential_part(&a2.1, &update).unwrap().unwrap();
    assert!(base_generation_missing(&save, REQUIRE_BASE_GENERATION));
}

/// 清除 AppSecret keeps the AppID and the openid (no rebind) on every node;
/// a later new AppSecret resumes sending with the same openid, no rebind.
#[test]
fn a_cleared_then_resaved_secret_resumes_with_the_same_openid_and_no_rebind() {
    use crate::notify::readiness::{missing_synced, ClusterHas, SyncState};
    use crate::notify::store::accept_copy_as;
    let (a, b) = (holder("a"), temp());
    accept_copy_as(&b.1, &export_copy(&a.1).unwrap(), None, NOW).unwrap();
    let openid_of = |s: &Store| export_copy(s).unwrap().openid.clone();
    let openid_before = openid_of(&b.1);
    assert!(!openid_before.is_empty());
    let codes = |s: &Store| {
        let meta = load_meta(s).unwrap();
        let view = view(s).unwrap();
        assert_eq!(view["openid_rebind"], false, "never a rebind");
        missing_synced(
            true,
            (
                !appid(s).is_empty(),
                secret(s).is_some(),
                load_openid(s).unwrap().is_set(),
            ),
            Some(true),
            SyncState {
                generation: Some(meta.generation),
                rebind: false,
            },
            ClusterHas {
                generation: meta.generation,
                appid: true,
                appsecret: secret(s).is_some(),
            },
        )
    };
    assert!(codes(&b.1).is_empty(), "capable before");
    let gen = load_meta(&a.1).unwrap().generation;
    let mut clear = save_req(Some(gen), None, None);
    clear.clear_appsecret = true;
    apply_save(&a.1, &clear, "a", NOW).unwrap();
    accept_copy_as(&b.1, &export_copy(&a.1).unwrap(), None, NOW).unwrap();
    for s in [&a.1, &b.1] {
        assert_eq!(load_meta(s).unwrap().generation, gen + 1);
        assert_eq!(appid(s), APPID, "AppID kept");
        assert_eq!(secret(s), None, "secret cleared everywhere");
        assert!(load_openid(s).unwrap().is_set(), "openid kept");
        assert_eq!(codes(s), vec!["appsecret"]);
    }
    let mut resave = save_req(Some(gen + 1), None, Some(SECRET2));
    resave.clear_appsecret = false;
    apply_save(&a.1, &resave, "a", NOW).unwrap();
    accept_copy_as(&b.1, &export_copy(&a.1).unwrap(), None, NOW).unwrap();
    for s in [&a.1, &b.1] {
        assert_eq!(secret(s).as_deref(), Some(SECRET2));
        assert!(codes(s).is_empty(), "notify-capable again");
    }
    for s in [&a.1, &b.1] {
        assert!(openid_of(s) == openid_before, "same openid, no rebind");
    }
}

/// Rule 1: an app_secret-only PUT and a 清除 PUT without a top-level
/// `base_generation` each get 400 `base_generation_required`, and nothing
/// changes.
#[tokio::test]
async fn a_save_or_clear_without_base_generation_gets_400() {
    use crate::webui::api::notify::{base_generation_required, credential_part, UpdateNotify};
    let a = holder("a");
    let before = snapshot(&a.1);
    let rev = load_secret(&a.1).unwrap().1;
    for body in [
        serde_json::json!({"app_secret": SECRET2}),
        serde_json::json!({"clear_app_secret": true, "expected_secret_revision": rev}),
    ] {
        let update: UpdateNotify = serde_json::from_value(body).unwrap();
        let save = credential_part(&a.1, &update).unwrap().unwrap();
        assert!(base_generation_missing(&save, REQUIRE_BASE_GENERATION));
        let response = base_generation_required();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["data"]["code"], "base_generation_required");
    }
    assert_eq!(snapshot(&a.1), before);
}
