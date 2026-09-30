use super::*;
use serde_json::json;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bilistream-storage-test-{}",
            paths::hex(&crypto::random::<16>().unwrap())
        ));
        fs::create_dir_all(path.join("legacy")).unwrap();
        Self(path)
    }
    fn open(&self) -> io::Result<Arc<Store>> {
        Store::open(
            self.0.join("data"),
            self.0.join("keys/master.key"),
            Some(self.0.join("legacy")),
        )
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
fn migrates_preserves_extensions_and_encrypts_before_sqlite() {
    let dir = Directory::new();
    let value = config_fixture();
    fs::write(
        dir.0.join("legacy/config.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let store = dir.open().unwrap();
    assert_eq!(store.read("config.json").unwrap().unwrap().value, value);
    assert!(!dir.0.join("legacy/config.json").exists());
    for entry in fs::read_dir(dir.0.join("data")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            assert!(!fs::read(path)
                .unwrap()
                .windows(23)
                .any(|s| s == b"synthetic-secret-marker"));
        }
    }
    drop(store);
    let reopened = dir.open().unwrap();
    assert_eq!(reopened.read("config.json").unwrap().unwrap().value, value);
}

#[test]
fn malformed_migration_keeps_original_and_retries_without_reset() {
    let dir = Directory::new();
    let path = dir.0.join("legacy/config.json");
    fs::write(&path, b"{broken").unwrap();
    assert!(dir.open().is_err());
    assert_eq!(fs::read(&path).unwrap(), b"{broken");
    fs::write(&path, serde_json::to_vec(&config_fixture()).unwrap()).unwrap();
    let store = dir.open().unwrap();
    assert_eq!(
        store.read("config.json").unwrap().unwrap().value["extension"]["keep"],
        true
    );
}

#[test]
fn legacy_export_preserves_latest_state_times_and_local_cookie_paths() {
    let dir = Directory::new();
    let store = dir.open().unwrap();
    let mut config = config_fixture();
    config["youtube"]["cookies_file"] = json!("/old/shared/cookies.txt");
    store.write("config.json", config).unwrap();
    let jar = "# Netscape HTTP Cookie File\n.example.invalid\tTRUE\t/\tTRUE\t0\tsession\tsynthetic-only\n";
    store.transaction(move |tx| {
        tx.write_at("cookies.txt", json!(jar), 1000)?;
        tx.write("youtube_quota.json", json!({"day":42,"keys":{"fixture":{"used":8500,"exhausted_until":43}}}))?;
        tx.write("youtube_golive_hours.json", json!({"channels":{"fixture":vec![2.0;24]},"counted":{"fixture-video":123},"decayed_at":100}))?;
        Ok(())
    }).unwrap();
    let output = dir.0.join("downgrade");
    store.export_legacy(&output).unwrap();
    assert_eq!(fs::read_to_string(output.join("cookies.txt")).unwrap(), jar);
    assert_eq!(
        fs::metadata(output.join("cookies.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        UNIX_EPOCH + Duration::from_secs(1000)
    );
    let config: Value =
        serde_json::from_slice(&fs::read(output.join("config.json")).unwrap()).unwrap();
    assert_eq!(config["youtube"]["cookies_file"], "cookies.txt");
    assert_eq!(config["extension"]["keep"], true);
    assert!(store.export_legacy(&output).is_err());
    // Round-trip through the real importer, including quota and learned hours.
    let restored = Store::open(
        dir.0.join("restored"),
        dir.0.join("keys/restored.key"),
        Some(output),
    )
    .unwrap();
    for name in [
        "cookies.txt",
        "youtube_quota.json",
        "youtube_golive_hours.json",
    ] {
        assert_eq!(
            restored.read(name).unwrap().unwrap().value,
            store.read(name).unwrap().unwrap().value
        );
    }
}

#[test]
fn learned_hour_weights_keep_f64_precision_through_migration_and_reopen() {
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
    fs::write(dir.0.join("legacy/youtube_golive_hours.json"), raw).unwrap();
    fs::write(
        dir.0.join("legacy/config.json"),
        serde_json::to_vec(&config_fixture()).unwrap(),
    )
    .unwrap();
    for pass in 0..2 {
        let store = dir.open().unwrap();
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
        assert_eq!(bits, expected, "migration/reopen pass {pass}");
        if pass == 1 {
            let export = dir.0.join("export");
            store.export_legacy(&export).unwrap();
            let text = fs::read_to_string(export.join("youtube_golive_hours.json")).unwrap();
            for decimal in &decimals {
                assert!(text.contains(decimal), "export changed a stored weight");
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
