use super::*;

fn catalog() -> WatchCatalog {
    WatchCatalog::from_value(Some(json!({"channels":[
        {"name":"主频道","niconico_name":"别名直播间","aliases":["别名"],
         "platforms":{"youtube":"UCconfigured","twitch":"SomeLogin","niconico":"nico-live"}},
        {"name":"另一频道","aliases":[],"platforms":{"youtube":"UCanother"}}
    ]})))
    .unwrap()
}

fn parse_watch(entries: Value) -> WatchConfig {
    serde_json::from_value(json!({"enabled":true,"channels":entries})).unwrap()
}

#[test]
fn legacy_migration_uses_accounts_and_keeps_display_aliases_in_one_channel() {
    let mut value = json!({"watch":{"enabled":true,"channels":[
        {"platform":"youtube","channel_id":"UCconfigured","name":"旧名称"},
        {"platform":"twitch","channel_id":"SOMELOGIN","name":"另一个显示名"},
        {"platform":"niconico","channel_id":"nico-live","name":"别名直播间"},
        {"platform":"twitch","channel_id":"somelogin","name":"重复项"},
        // Same display name alone must never select the configured channel.
        {"platform":"youtube","channel_id":"UCunrelated","name":"主频道"},
        {"platform":"twitch","channel_id":"UnrelatedLogin","name":"主频道"}
    ]}});
    assert!(migrate_watch(&mut value, &catalog(), 100).unwrap());
    assert_eq!(
        value["watch"]["channels"],
        json!([
            {"kind":"configured","name":"主频道"},
            {"kind":"manual","name":"主频道","platforms":{"youtube":"UCunrelated","twitch":"unrelatedlogin"}}
        ])
    );
    assert_eq!(value["watch"]["enabled"], true);
    assert_eq!(
        value["watch"]["migration_notice"],
        json!({"at":100,"folded":6})
    );
    let once = value.clone();
    assert!(!migrate_watch(&mut value, &catalog(), 200).unwrap());
    assert_eq!(value, once);
    let watch: WatchConfig = serde_json::from_value(value["watch"].clone()).unwrap();
    validate_watch(&watch, &catalog(), &watch).unwrap();
}

#[test]
fn migration_keeps_every_unmatched_target_despite_blank_long_and_colliding_names() {
    let long = "長".repeat(60);
    let mut value = json!({"watch":{"channels":[
        {"platform":"youtube","channel_id":"UCfirst","name":"同名"},
        {"platform":"youtube","channel_id":"UCsecond","name":"同名"},
        {"platform":"twitch","channel_id":"Partner","name":"同名"},
        {"platform":"niconico","channel_id":"blank-name","name":" \n "},
        {"platform":"youtube","channel_id":"UClong1","name":long},
        {"platform":"youtube","channel_id":"UClong2","name":long}
    ]}});
    migrate_watch(&mut value, &WatchCatalog::default(), 100).unwrap();
    let watch: WatchConfig = serde_json::from_value(value["watch"].clone()).unwrap();
    validate_watch(&watch, &WatchCatalog::default(), &watch).unwrap();
    assert_eq!(watch.channels.len(), 5);
    assert_eq!(watch.channels[0].name(), "同名");
    assert_eq!(watch.channels[1].name(), "同名 (2)");
    assert_eq!(watch.channels[2].name(), "blank-name");
    assert!(watch
        .channels
        .iter()
        .all(|entry| !entry.name().is_empty() && entry.name().chars().count() <= 40));
    let accounts: HashSet<_> = watch
        .channels
        .iter()
        .flat_map(|entry| {
            entry
                .platforms(&WatchCatalog::default())
                .unwrap()
                .identities()
        })
        .collect();
    assert_eq!(
        accounts,
        HashSet::from([
            "youtube:UCfirst".into(),
            "youtube:UCsecond".into(),
            "twitch:partner".into(),
            "niconico:blank-name".into(),
            "youtube:UClong1".into(),
            "youtube:UClong2".into(),
        ])
    );
    let once = value.clone();
    migrate_watch(&mut value, &WatchCatalog::default(), 200).unwrap();
    assert_eq!(value, once);
}

#[test]
fn configured_accounts_are_live_catalog_references_and_unresolved_entries_are_retained() {
    let entry = WatchChannel::Configured {
        name: "主频道".into(),
    };
    let mut roster = catalog();
    assert_eq!(
        entry.platforms(&roster).unwrap().youtube.as_deref(),
        Some("UCconfigured")
    );
    roster.channels.get_mut("主频道").unwrap().youtube = Some("UCnew".into());
    assert_eq!(
        entry.platforms(&roster).unwrap().youtube.as_deref(),
        Some("UCnew")
    );
    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({"kind":"configured","name":"主频道"})
    );
    let existing = parse_watch(json!([{"kind":"configured","name":"主频道"}]));
    roster.channels.remove("主频道");
    assert!(entry.platforms(&roster).is_none());
    validate_watch(&existing, &roster, &existing).unwrap();
    assert_eq!(
        validate_watch(&existing, &roster, &WatchConfig::default())
            .unwrap_err()
            .code,
        "watch_unknown_channel"
    );
    let candidate = catalog().view();
    assert!(
        candidate.contains(&json!({"name":"主频道","platforms":["youtube","twitch","niconico"]}))
    );
    assert!(!candidate.iter().any(|entry| entry["name"] == "别名直播间"));
}

#[test]
fn validation_rejects_duplicate_names_and_overlapping_accounts_including_twitch_case() {
    let mut roster = catalog();
    roster.channels.insert(
        "overlap".into(),
        WatchPlatforms {
            twitch: Some("SOMELOGIN".into()),
            ..Default::default()
        },
    );
    for entries in [
        json!([{"kind":"configured","name":"主频道"},{"kind":"configured","name":"主频道"}]),
        json!([{"kind":"manual","name":"same","platforms":{"youtube":"UC1"}},
               {"kind":"manual","name":"same","platforms":{"twitch":"other"}}]),
        json!([{"kind":"configured","name":"主频道"},
               {"kind":"configured","name":"overlap"}]),
        json!([{"kind":"manual","name":"x","platforms":{"twitch":"SOMELOGIN"}},
               {"kind":"manual","name":"y","platforms":{"twitch":"somelogin"}}]),
    ] {
        assert_eq!(
            validate_watch(&parse_watch(entries), &roster, &WatchConfig::default())
                .unwrap_err()
                .code,
            "watch_duplicate"
        );
    }
    // Historical inactive data is retained separately from active selections.
    let distinct = parse_watch(json!([
        {"kind":"configured","name":"主频道"},
        {"kind":"manual","name":"主频道","platforms":{"youtube":"UCdistinct"}}
    ]));
    validate_watch(&distinct, &catalog(), &distinct).unwrap();
}

#[test]
fn fifty_grouped_entries_can_have_three_accounts_each() {
    let mut roster = catalog();
    let entries: Vec<_> = (0..50)
        .map(|index| {
            let name = format!("channel-{index}");
            roster.channels.insert(
                name.clone(),
                WatchPlatforms {
                    youtube: Some(format!("UC{index}")),
                    twitch: Some(format!("login{index}")),
                    niconico: Some(format!("nico{index}")),
                },
            );
            json!({"kind":"configured","name":name})
        })
        .collect();
    let mut watch = parse_watch(json!(entries));
    validate_watch(&watch, &roster, &WatchConfig::default()).unwrap();
    watch.channels.push(WatchChannel::Configured {
        name: "另一频道".into(),
    });
    assert_eq!(
        validate_watch(&watch, &roster, &WatchConfig::default())
            .unwrap_err()
            .code,
        "watch_too_many"
    );
}

#[test]
fn manual_names_and_accounts_are_validated_without_guessing_invalid_targets() {
    for entry in [
        json!({"kind":"manual","name":"","platforms":{"youtube":"UC1"}}),
        json!({"kind":"manual","name":"a\nb","platforms":{"youtube":"UC1"}}),
        json!({"kind":"manual","name":"名".repeat(41),"platforms":{"youtube":"UC1"}}),
        json!({"kind":"manual","name":"x","platforms":{}}),
        json!({"kind":"manual","name":"x","platforms":{"youtube":""}}),
        json!({"kind":"manual","name":"x","platforms":{"twitch":"https://twitch.tv/login"}}),
    ] {
        let watch = parse_watch(json!([entry]));
        assert_eq!(
            validate_watch(&watch, &catalog(), &watch).unwrap_err().code,
            "invalid_field"
        );
    }
    assert!(serde_json::from_value::<WatchConfig>(
        json!({"channels":[{"kind":"manual","name":"x","platforms":{"other":"x"}}]})
    )
    .is_err());
}
