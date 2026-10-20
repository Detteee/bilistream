use super::*;
use serde_json::json;

fn schema_version(store: &Store) -> u32 {
    Connection::open(store.data_dir().join("bilistream.db"))
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn a_fresh_store_seeds_an_empty_player_filter_document() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    assert_eq!(
        store.read("invalid_words.txt").unwrap().unwrap().value,
        json!(""),
        "the player-filter settings page reads this document and errors when it is absent"
    );
}

#[test]
fn first_protected_write_upgrades_atomically_and_reopen_never_downgrades() {
    for name in MANAGED_RECORDS {
        let dir = Directory::new();
        let store = dir.open().unwrap();
        assert_eq!(schema_version(&store), 1);
        let name = name.to_string();
        let aborted = name.clone();
        let result: io::Result<()> = store.transaction(move |tx| {
            tx.write(&aborted, json!({"lifecycle":"join_ready"}))?;
            Err(io::Error::other("synthetic transaction failure"))
        });
        assert!(result.is_err());
        assert_eq!(schema_version(&store), 1);
        assert!(store.read(&name).unwrap().is_none());
        store
            .write(&name, json!({"lifecycle":"join_ready"}))
            .unwrap();
        assert_eq!(schema_version(&store), 2);
        drop(store);
        let reopened = dir.open().unwrap();
        assert_eq!(schema_version(&reopened), 2);
        assert!(reopened.read(&name).unwrap().is_some());
    }
}

#[test]
fn newer_schema_is_rejected_without_lowering_it() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    let database = store.data_dir().join("bilistream.db");
    drop(store);
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch("PRAGMA user_version=4").unwrap();
    assert!(dir.open().is_err());
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        4
    );
}

#[test]
fn fencing_schema_is_atomic_and_cannot_be_lowered_by_later_membership_writes() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    let failed: io::Result<()> = store.transaction(|tx| {
        tx.require_fencing_schema()?;
        tx.write("cluster-local", json!({"lifecycle":"join_ready"}))?;
        Err(io::Error::other("synthetic fencing policy failure"))
    });
    assert!(failed.is_err());
    assert_eq!(schema_version(&store), 1);
    assert!(store.read("cluster-local").unwrap().is_none());
    store
        .transaction(|tx| {
            tx.require_fencing_schema()?;
            tx.write("cluster-local", json!({"lifecycle":"join_ready"}))
        })
        .unwrap();
    assert_eq!(schema_version(&store), 3);
    for name in MANAGED_RECORDS {
        store.write(name, json!({"synthetic":"record"})).unwrap();
        assert_eq!(schema_version(&store), 3);
    }
    drop(store);
    let reopened = dir.open().unwrap();
    assert_eq!(schema_version(&reopened), 3);
}

#[test]
fn protected_and_legacy_clusters_export_unjoined_and_off_without_changing_source() {
    for lifecycle in [
        None,
        Some("join_ready"),
        Some("pairing"),
        Some("managed"),
        Some("left"),
        Some("incompatible_held"),
    ] {
        let dir = Directory::new();
        let store = dir.open().unwrap();
        let mut config = config_fixture();
        config["cluster"] = json!({"enabled":true,"node_id":"node","peers":[{"node_id":"peer","api_url":"https://example.invalid"}],"public_status":{"node_id":"node"}});
        for platform in ["youtube", "twitch", "niconico"] {
            config[platform] = json!({"enable_monitor":true});
        }
        config["priority_channel"] = json!({"enabled":true,"auto_restart":true});
        store.write("config.json", config.clone()).unwrap();
        if let Some(lifecycle) = lifecycle {
            store
                .write(
                    "cluster-local",
                    json!({"lifecycle":lifecycle,"hold":"operation"}),
                )
                .unwrap();
            store
                .write(
                    "cluster-identity",
                    json!({"private_key":"synthetic-private-identity"}),
                )
                .unwrap();
            store
                .write("cluster-operations", json!({"private":"synthetic-journal"}))
                .unwrap();
        }
        let backup = store.export_backup("synthetic-backup-password").unwrap();
        let decoded = crypto::restore("synthetic-backup-password", &backup).unwrap();
        let docs: HashMap<String, Document> = serde_json::from_slice(&decoded).unwrap();
        assert!(MANAGED_RECORDS.iter().all(|name| !docs.contains_key(*name)));
        let exported = &docs["config.json"].value;
        assert_eq!(exported["cluster"]["enabled"], false);
        assert_eq!(exported["cluster"]["peers"], json!([]));
        assert_eq!(exported["cluster"]["public_status"]["node_id"], "");
        for platform in ["youtube", "twitch", "niconico"] {
            assert_eq!(exported[platform]["enable_monitor"], false);
        }
        assert_eq!(exported["priority_channel"]["enabled"], false);
        assert_eq!(exported["priority_channel"]["auto_restart"], false);
        assert_eq!(store.read("config.json").unwrap().unwrap().value, config);
    }
}

#[test]
fn restore_old_cluster_backup_into_running_empty_store_is_immediately_safe() {
    for enabled in [true, false] {
        let source = Directory::new();
        let source_store = source.open().unwrap();
        let mut config = config_fixture();
        config["cluster"] = json!({"enabled":enabled,"node_id":"old-node","peers":[{"node_id":"old-peer","api_url":"https://example.invalid"}],"public_status":{"node_id":"old-node"}});
        for platform in ["youtube", "twitch", "niconico"] {
            config[platform] = json!({"enable_monitor":true});
        }
        config["priority_channel"] = json!({"enabled":true,"auto_restart":true});
        source_store.write("config.json", config.clone()).unwrap();
        // Model a backup produced before portable exports sanitized cluster
        // settings. Using today's export_backup would hide the import defect.
        let bytes = serde_json::to_vec(&source_store.snapshot().unwrap()).unwrap();
        let backup = crypto::export("synthetic-password", &bytes).unwrap();
        let destination = Directory::new();
        let store = destination.open().unwrap();
        let membership = crate::cluster::membership::Membership::new(store.clone());
        membership.initialize().unwrap();
        store.restore_backup("synthetic-password", &backup).unwrap();
        let restored = store.read("config.json").unwrap().unwrap().value;
        if enabled {
            assert_eq!(restored["cluster"]["enabled"], false);
            assert_eq!(restored["cluster"]["peers"], json!([]));
            assert_eq!(restored["cluster"]["public_status"]["node_id"], "");
            for platform in ["youtube", "twitch", "niconico"] {
                assert_eq!(restored[platform]["enable_monitor"], false);
            }
            assert_eq!(restored["priority_channel"]["enabled"], false);
            assert_eq!(restored["priority_channel"]["auto_restart"], false);
            assert_eq!(restored["bililive"], config["bililive"]);
            assert_eq!(restored["extension"], config["extension"]);
        } else {
            // Ordinary standalone restores retain their monitoring choices.
            assert_eq!(restored, config);
        }
        assert_eq!(schema_version(&store), 1);
        assert!(store.read("cluster-local").unwrap().is_none());
        membership.initialize().unwrap();
        assert!(store.read("cluster-local").unwrap().is_none());
    }
}

#[test]
fn restore_refuses_pending_state_and_preserves_allowed_destination_identity_and_hold() {
    let source = Directory::new();
    let source_store = source.open().unwrap();
    let mut config = config_fixture();
    config["youtube"]["enable_monitor"] = json!(true);
    source_store.write("config.json", config).unwrap();
    let backup = source_store.export_backup("synthetic-password").unwrap();
    for lifecycle in ["pairing", "managed", "unknown"] {
        let dir = Directory::new();
        let store = dir.open().unwrap();
        store
            .write(
                "cluster-local",
                json!({"lifecycle":lifecycle,"hold":"pending"}),
            )
            .unwrap();
        assert!(store.restore_backup("synthetic-password", &backup).is_err());
        assert!(store.read("config.json").unwrap().is_none());
    }
    let dir = Directory::new();
    let store = dir.open().unwrap();
    let identity = json!({"private_key":"destination-identity"});
    let local = json!({"lifecycle":"join_ready","hold":"join"});
    let password = json!({"version":1,"password":"destination-password"});
    store.write("cluster-identity", identity.clone()).unwrap();
    store.write("cluster-local", local.clone()).unwrap();
    store.write("webui-password", password.clone()).unwrap();
    store.restore_backup("synthetic-password", &backup).unwrap();
    assert_eq!(
        store.read("cluster-identity").unwrap().unwrap().value,
        identity
    );
    assert_eq!(store.read("cluster-local").unwrap().unwrap().value, local);
    assert_eq!(
        store.read("webui-password").unwrap().unwrap().value,
        password
    );
    assert_eq!(
        store.read("config.json").unwrap().unwrap().value["youtube"]["enable_monitor"],
        false
    );
    assert_eq!(schema_version(&store), 2);
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bilistream-storage-test-{}",
            paths::hex(&crypto::random::<16>().unwrap())
        ));
        Self(path)
    }
    fn open(&self) -> io::Result<Arc<Store>> {
        Store::open(self.0.join("data"), self.0.join("keys/master.key"))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn config_fixture() -> Value {
    json!({"auto_cover":false,"enable_anti_collision":false,"interval":15,"bililive":{"room":1,"enable_danmaku_command":false,"bili_rtmp_url":"","bili_rtmp_key":"synthetic-secret-marker"},"youtube":{},"twitch":{},"enable_lol_monitor":false,"anti_collision_list":{},"extension":{"keep":true}})
}

#[test]
fn learned_hour_weights_keep_f64_precision_through_backup_and_reopen() {
    let dir = Directory::new();
    let decimals = [
        "1.4602988135614665",
        "1.7576220721073945",
        "0.9095704406102849",
    ]
    .repeat(8);
    let expected: Vec<u64> = decimals
        .iter()
        .map(|text| text.parse::<f64>().unwrap().to_bits())
        .collect();
    let raw = format!(
        r#"{{"channels":{{"fixture":[{}]}},"counted":{{"fixture-video":123}},"decayed_at":100}}"#,
        decimals.join(",")
    );
    let learned: Value = serde_json::from_str(&raw).unwrap();
    let store = dir.open().unwrap();
    store.write("youtube_golive_hours.json", learned).unwrap();
    for pass in 0..2 {
        let value = store
            .read("youtube_golive_hours.json")
            .unwrap()
            .unwrap()
            .value;
        let bits: Vec<u64> = value["channels"]["fixture"]
            .as_array()
            .unwrap()
            .iter()
            .map(|number| number.as_f64().unwrap().to_bits())
            .collect();
        assert_eq!(bits, expected, "write/reopen pass {pass}");
        if pass == 1 {
            // The encrypted backup is the surviving JSON boundary.
            let backup = store.export_backup("synthetic-password").unwrap();
            let decoded = crypto::restore("synthetic-password", &backup).unwrap();
            let docs: HashMap<String, Document> = serde_json::from_slice(&decoded).unwrap();
            let text = docs["youtube_golive_hours.json"].value.to_string();
            for decimal in &decimals {
                assert!(text.contains(decimal), "backup changed a stored weight");
            }
        }
    }
}

#[test]
fn missing_or_wrong_key_never_reinitializes_existing_database() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    store.write("config.json", config_fixture()).unwrap();
    drop(store);
    let key_path = dir.0.join("keys/master.key");
    let key = fs::read(&key_path).unwrap();
    fs::remove_file(&key_path).unwrap();
    assert!(dir.open().is_err());
    assert!(!key_path.exists());
    paths::write_private(&key_path, &[7; 32]).unwrap();
    assert!(dir.open().is_err());
    fs::write(&key_path, key).unwrap();
    assert_eq!(
        dir.open()
            .unwrap()
            .read("config.json")
            .unwrap()
            .unwrap()
            .value,
        config_fixture()
    );
}

#[test]
fn transaction_rollback_and_cas_keep_published_values_current() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    let first = store.write("counter", json!(0)).unwrap();
    let failed: io::Result<()> = store.transaction(|tx| {
        tx.write("counter", json!(9))?;
        tx.write("another", json!(true))?;
        Err(io::Error::other("synthetic failure"))
    });
    assert!(failed.is_err());
    assert_eq!(store.read("counter").unwrap().unwrap().value, 0);
    assert!(store.read("another").unwrap().is_none());
    let revision = store.compare_exchange("counter", first, json!(1)).unwrap();
    assert!(revision > first);
    assert_eq!(
        store
            .compare_exchange("counter", first, json!(2))
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(store.read("counter").unwrap().unwrap().value, 1);
}

#[test]
fn concurrent_updates_are_serialized_and_instance_is_exclusive() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    assert!(dir.open().is_err());
    store.write("counter", json!(0)).unwrap();
    let workers: Vec<_> = (0..12)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .transaction(|tx| {
                        let current = tx.read("counter")?.unwrap().value.as_u64().unwrap();
                        tx.write("counter", json!(current + 1))
                    })
                    .unwrap()
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(store.read("counter").unwrap().unwrap().value, 12);
}

#[test]
fn portable_backup_restores_using_different_machine_key() {
    let first = Directory::new();
    let store = first.open().unwrap();
    store.write("config.json", config_fixture()).unwrap();
    store
        .write(
            "youtube_quota.json",
            json!({"day":42,"keys":{"synthetic-fingerprint":{"used":8500,"exhausted_until":43}}}),
        )
        .unwrap();
    let backup = store.export_backup("synthetic backup password").unwrap();
    assert!(!backup.windows(23).any(|b| b == b"synthetic-secret-marker"));
    let second = Directory::new();
    let restored = second.open().unwrap();
    assert!(restored
        .restore_backup("incorrect password", &backup)
        .is_err());
    restored
        .restore_backup("synthetic backup password", &backup)
        .unwrap();
    assert_eq!(
        restored.read("config.json").unwrap().unwrap().value,
        config_fixture()
    );
    assert_eq!(
        restored.read("youtube_quota.json").unwrap().unwrap().value["keys"]
            ["synthetic-fingerprint"]["used"],
        8500
    );
    assert!(restored
        .restore_backup("synthetic backup password", &backup)
        .is_err());
}

#[test]
fn authenticated_envelopes_reject_tamper_and_record_swap() {
    let key = Key([1; 32]);
    let mut data = crypto::seal(&key, b"record-a", b"synthetic-private-value").unwrap();
    assert!(crypto::open(&key, b"record-b", &data).is_err());
    *data.last_mut().unwrap() ^= 1;
    assert!(crypto::open(&key, b"record-a", &data).is_err());
}

/// 28a20792 still knew the Niconico cookie file: its config may carry
/// `niconico.cookies_file` and its store or backups a `niconico_cookies.txt`
/// document. This build ignores both: the config loads, the document is
/// never read or exported, and a 28a20792 backup restores without it.
fn config_with_niconico_cookie_file() -> Value {
    let mut config = serde_json::to_value(crate::test_support::config("local", 0)).unwrap();
    config["niconico"]["cookies_file"] = json!("niconico_cookies.txt");
    config
}

const SYNTHETIC_NETSCAPE: &str =
    "# Netscape HTTP Cookie File\n.nicovideo.jp\tTRUE\t/\tTRUE\t0\tuser_session\tsynthetic-nico-session\n";

#[test]
fn a_28a20792_config_with_a_niconico_cookie_file_key_still_loads() {
    let config = config_with_niconico_cookie_file();
    documents::validate("config.json", &config).unwrap();
    let loaded: crate::config::Config = serde_json::from_value(config).unwrap();
    let saved = serde_json::to_value(&loaded).unwrap();
    assert!(
        saved["niconico"].get("cookies_file").is_none(),
        "the key is dropped on the next save"
    );
    assert!(loaded.niconico.user_session.is_none());
}

#[test]
fn a_leftover_niconico_cookie_document_is_never_exported() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    store
        .write("config.json", config_with_niconico_cookie_file())
        .unwrap();
    store
        .write("niconico_cookies.txt", json!(SYNTHETIC_NETSCAPE))
        .unwrap();
    let backup = store.export_backup("synthetic-backup-password").unwrap();
    let plain = crypto::restore("synthetic-backup-password", &backup).unwrap();
    let docs: HashMap<String, Document> = serde_json::from_slice(&plain).unwrap();
    assert!(!docs.contains_key("niconico_cookies.txt"));
    assert!(docs.contains_key("config.json"));
    assert!(!String::from_utf8_lossy(&plain).contains("synthetic-nico-session"));
    assert!(
        store.read("niconico_cookies.txt").unwrap().is_some(),
        "the stored copy is left untouched"
    );
}

#[test]
fn a_28a20792_backup_with_a_niconico_cookie_document_restores_without_it() {
    let docs = HashMap::from([
        (
            "config.json".to_string(),
            Document {
                value: config_with_niconico_cookie_file(),
                revision: 3,
                updated_at: 1_791_702_240,
            },
        ),
        (
            "niconico_cookies.txt".to_string(),
            Document {
                value: json!(SYNTHETIC_NETSCAPE),
                revision: 1,
                updated_at: 1_791_702_240,
            },
        ),
    ]);
    let backup = crypto::export(
        "synthetic-backup-password",
        &serde_json::to_vec(&docs).unwrap(),
    )
    .unwrap();
    let dir = Directory::new();
    let store = dir.open().unwrap();
    store
        .restore_backup("synthetic-backup-password", &backup)
        .unwrap();
    assert!(store.read("config.json").unwrap().is_some());
    assert!(store.read("niconico_cookies.txt").unwrap().is_none());
}

#[test]
fn warning_exempt_rail_document_validates_its_shape() {
    use super::documents::validate;
    validate(
        "warning_exempt_rail.json",
        &json!({"restarts":[{"platform":"YT","channel_id":"UC1","at":1,"reason":"idle_recording"}]}),
    )
    .unwrap();
    validate("warning_exempt_rail.json", &json!({"restarts":[]})).unwrap();
    assert!(validate("warning_exempt_rail.json", &json!({"restarts":"x"})).is_err());
    assert!(validate(
        "warning_exempt_rail.json",
        &json!({"restarts":[{"platform":"YT"}]})
    )
    .is_err());
}

#[test]
fn old_format_channel_keys_are_refused_by_name_without_their_content() {
    use super::documents::{reject_legacy_channels, validate, LEGACY_CHANNEL_KEYS};
    let current = json!({"channels":[{"name":"SyntheticCurrent","platforms":{"youtube":"UCsyntheticCurrent0001"},"aliases":[]}]});
    validate("channels.json", &current).unwrap();
    reject_legacy_channels(&current).unwrap();
    for key in LEGACY_CHANNEL_KEYS {
        for doc in [
            // Old format only, and old keys left beside the current list.
            {
                let mut old = json!({});
                old[key] = json!([{"channel_name":"SyntheticLegacyName","channel_id":"UCsyntheticLegacyId0001"}]);
                old
            },
            {
                let mut mixed = current.clone();
                mixed[key] = json!([{"channel_name":"SyntheticLegacyName","channel_id":"UCsyntheticLegacyId0001"}]);
                mixed
            },
        ] {
            for error in [
                validate("channels.json", &doc).unwrap_err(),
                reject_legacy_channels(&doc).unwrap_err(),
            ] {
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                let text = error.to_string();
                assert!(
                    text.contains("channels.json") && text.contains(key),
                    "{text}"
                );
                for leaked in [
                    "SyntheticLegacyName",
                    "UCsyntheticLegacyId0001",
                    "SyntheticCurrent",
                ] {
                    assert!(!text.contains(leaked), "{text}");
                    assert!(!format!("{error:?}").contains(leaked));
                }
            }
        }
    }
}
