use super::tests::{test_config, ClusterStateGuard};
use super::*;

#[test]
fn indirect_quorum_can_keep_peer_eligible_without_refreshing_heartbeat() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "d".to_string(),
            name: "d".to_string(),
            api_url: "http://d".to_string(),
            priority: 3,
        },
    ];
    let now = now_secs();
    let mut state = ClusterState::default();
    let mut b = empty_node("b", "b", "http://b", 10, false, now);
    b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
    b.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
    state.nodes.insert("b".to_string(), b);

    // Observers must themselves be reliable nodes for their vouching to count.
    let mut c = empty_node("c", "c", "http://c", 5, false, now);
    c.last_seen = Some(now);
    c.health = ClusterHealth::healthy();
    state.nodes.insert("c".to_string(), c);
    let mut d = empty_node("d", "d", "http://d", 3, false, now);
    d.last_seen = Some(now);
    d.health = ClusterHealth::healthy();
    state.nodes.insert("d".to_string(), d);

    state
        .peer_observations
        .entry("b".to_string())
        .or_default()
        .insert("c".to_string(), now);
    state
        .peer_observations
        .entry("b".to_string())
        .or_default()
        .insert("d".to_string(), now);

    assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
    assert!(is_stale(state.nodes.get("b").unwrap(), &cfg, now));
}

#[test]
fn indirect_quorum_marks_peer_healthy_but_stale_for_status() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "d".to_string(),
            name: "d".to_string(),
            api_url: "http://d".to_string(),
            priority: 3,
        },
    ];
    let now = now_secs();
    let mut state = ClusterState::default();
    let mut b = empty_node("b", "b", "http://b", 10, false, now);
    b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
    state.nodes.insert("b".to_string(), b);

    for (node_id, priority) in [("c", 5), ("d", 3)] {
        let mut observer = empty_node(
            node_id,
            node_id,
            &format!("http://{}", node_id),
            priority,
            false,
            now,
        );
        observer.last_seen = Some(now);
        observer.health = ClusterHealth::healthy();
        state.nodes.insert(node_id.to_string(), observer);
        state
            .peer_observations
            .entry("b".to_string())
            .or_default()
            .insert(node_id.to_string(), now);
    }

    normalize_node_health(&mut state, &cfg, now);

    let node = state.nodes.get("b").unwrap();
    assert!(!node.health.healthy);
    assert!(node.health.stale);
    assert_eq!(node.health.reason, "heartbeat_timeout");
    assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
}

#[test]
fn single_indirect_observer_does_not_keep_peer_eligible_in_four_node_cluster() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "d".to_string(),
            name: "d".to_string(),
            api_url: "http://d".to_string(),
            priority: 3,
        },
    ];
    let now = now_secs();
    let mut state = ClusterState::default();
    let mut b = empty_node("b", "b", "http://b", 10, false, now);
    b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
    b.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
    state.nodes.insert("b".to_string(), b);
    state
        .peer_observations
        .entry("b".to_string())
        .or_default()
        .insert("c".to_string(), now);

    assert_eq!(choose_owner(&state, &cfg, now), None);
}

#[test]
fn indirect_observers_must_be_fresh_and_healthy() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "d".to_string(),
            name: "d".to_string(),
            api_url: "http://d".to_string(),
            priority: 3,
        },
    ];
    let now = now_secs();
    let mut state = ClusterState::default();

    let mut b = empty_node("b", "b", "http://b", 10, false, now);
    b.last_seen = Some(now - cfg.cluster.failover_timeout_secs - 1);
    b.health = ClusterHealth::unhealthy("heartbeat_timeout", true, false);
    state.nodes.insert("b".to_string(), b);

    let mut c = empty_node("c", "c", "http://c", 5, false, now);
    c.last_seen = Some(now);
    c.health = ClusterHealth::healthy();
    state.nodes.insert("c".to_string(), c);

    let mut d = empty_node("d", "d", "http://d", 3, false, now);
    d.last_seen = Some(now);
    d.health = ClusterHealth::healthy();
    state.nodes.insert("d".to_string(), d);

    state
        .peer_observations
        .entry("b".to_string())
        .or_default()
        .insert("c".to_string(), now);
    state
        .peer_observations
        .entry("b".to_string())
        .or_default()
        .insert("d".to_string(), now);

    assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));

    state.nodes.get_mut("d").unwrap().health = ClusterHealth::unhealthy("draining", false, false);
    assert_ne!(choose_owner(&state, &cfg, now), Some("b".to_string()));
}

#[test]
fn last_resort_does_not_promote_faulted_standby_with_preserved_toggles() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 10,
    }];

    let now = now_secs();
    let mut state = ClusterState {
        active_owner: Some("b".to_string()),
        ..ClusterState::default()
    };
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 10, false, now),
    );
    for node in state.nodes.values_mut() {
        node.network_unstable = true;
        node.health = ClusterHealth::unhealthy(NETWORK_ISOLATED_REASON, false, true);
        node.last_seen = Some(now);
    }

    assert!(monitor_toggles_any_enabled(
        &monitor_toggle_state_from_config(&cfg)
    ));
    assert_eq!(choose_owner(&state, &cfg, now), None);
}

#[test]
fn lease_selection_prefers_highest_healthy_priority() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 5,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 10,
        },
    ];

    let now = now_secs();
    let mut state = ClusterState::default();
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 5, false, now),
    );
    state.nodes.insert(
        "c".to_string(),
        empty_node("c", "c", "http://c", 10, false, now),
    );
    for node in state.nodes.values_mut() {
        node.health = ClusterHealth::healthy();
        node.last_seen = Some(now);
    }

    assert_eq!(choose_owner(&state, &cfg, now), Some("c".to_string()));

    state.nodes.get_mut("c").unwrap().last_seen = Some(now - 60);
    assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
}

#[test]
fn invalid_forced_owner_clears_to_priority_selection() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 5,
    }];

    let now = now_secs();
    let mut state = ClusterState {
        forced_owner: Some("a".to_string()),
        ..ClusterState::default()
    };
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 5, false, now),
    );
    for node in state.nodes.values_mut() {
        node.health = ClusterHealth::healthy();
        node.last_seen = Some(now);
    }
    state.nodes.get_mut("a").unwrap().draining = true;

    let configured = configured_node_ids(&cfg);
    clear_invalid_forced_owner(&mut state, &cfg, now, &configured);

    assert!(state.forced_owner.is_none());
    assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
}

#[test]
fn drain_replacement_is_planned_without_mutating_live_state() {
    let _guard = ClusterStateGuard::new();
    let mut cfg = test_config("a", 10);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 5,
    }];

    let now = now_secs();
    let mut state = ClusterState {
        active_owner: Some("a".to_string()),
        forced_owner: Some("a".to_string()),
        ..ClusterState::default()
    };
    for (node_id, priority, is_local) in [("a", 10, true), ("b", 5, false)] {
        let mut node = empty_node(node_id, node_id, "", priority, is_local, now);
        node.health = ClusterHealth::healthy();
        node.last_seen = Some(now);
        state.nodes.insert(node_id.to_string(), node);
    }
    *cluster_state_write() = state;

    assert_eq!(
        replacement_owner_for_drain(&cfg, "a"),
        Some("b".to_string())
    );
    let state = cluster_state_read();
    assert_eq!(state.forced_owner.as_deref(), Some("a"));
    assert!(!state.local_draining);
    assert!(!state.nodes["a"].draining);
}

#[test]
fn healthy_active_owner_is_not_preempted_by_higher_priority_peer() {
    let mut cfg = test_config("a", 10);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 1,
    }];

    let now = now_secs();
    let mut state = ClusterState {
        active_owner: Some("b".to_string()),
        ..ClusterState::default()
    };
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 10, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 1, false, now),
    );
    for node in state.nodes.values_mut() {
        node.health = ClusterHealth::healthy();
        node.last_seen = Some(now);
    }

    assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
}

#[test]
fn last_resort_keeps_local_when_all_peers_are_draining() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![
        crate::config::ClusterPeer {
            node_id: "b".to_string(),
            name: "b".to_string(),
            api_url: "http://b".to_string(),
            priority: 10,
        },
        crate::config::ClusterPeer {
            node_id: "c".to_string(),
            name: "c".to_string(),
            api_url: "http://c".to_string(),
            priority: 5,
        },
    ];

    let now = now_secs();
    let mut state = ClusterState::default();
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 10, false, now),
    );
    state.nodes.insert(
        "c".to_string(),
        empty_node("c", "c", "http://c", 5, false, now),
    );
    for node in state.nodes.values_mut() {
        node.last_seen = Some(now);
    }
    state.nodes.get_mut("a").unwrap().health = ClusterHealth::healthy();
    for peer_id in ["b", "c"] {
        let node = state.nodes.get_mut(peer_id).unwrap();
        node.draining = true;
        node.health = ClusterHealth::unhealthy("draining", false, false);
    }

    assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
}

#[test]
fn last_resort_keeps_local_when_temporarily_faulted_and_no_peer_available() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 10,
    }];

    let now = now_secs();
    let mut state = ClusterState {
        active_owner: Some("a".to_string()),
        ..ClusterState::default()
    };
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 10, false, now),
    );
    state.nodes.get_mut("a").unwrap().last_seen = Some(now);
    state.nodes.get_mut("a").unwrap().network_unstable = true;
    state.nodes.get_mut("a").unwrap().health =
        ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);
    let peer = state.nodes.get_mut("b").unwrap();
    peer.last_seen = Some(now);
    peer.draining = true;
    peer.health = ClusterHealth::unhealthy("draining", false, false);

    assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
}

#[test]
fn no_owner_when_local_is_operator_disabled_and_peers_unavailable() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 10,
    }];

    let now = now_secs();
    let mut state = ClusterState::default();
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 10, false, now),
    );
    state.nodes.get_mut("a").unwrap().last_seen = Some(now);
    state.nodes.get_mut("a").unwrap().draining = true;
    state.nodes.get_mut("a").unwrap().health = ClusterHealth::unhealthy("draining", false, false);
    let peer = state.nodes.get_mut("b").unwrap();
    peer.last_seen = Some(now);
    peer.draining = true;
    peer.health = ClusterHealth::unhealthy("draining", false, false);

    assert_eq!(choose_owner(&state, &cfg, now), None);
}

#[test]
fn stale_peer_owner_view_does_not_regress_local_view() {
    let mut state = ClusterState {
        active_owner: Some("a".to_string()),
        lease_until: 1_000,
        ..ClusterState::default()
    };

    let mut b = empty_node("b", "b", "http://b", 1, false, now_secs());
    b.health = ClusterHealth::healthy();
    state.nodes.insert("b".to_string(), b);

    adopt_owner_view(&mut state, Some("b".to_string()), 900, true, Some("c"));
    assert_eq!(state.active_owner.as_deref(), Some("a"));
    assert_eq!(state.lease_until, 1_000);

    adopt_owner_view(&mut state, Some("b".to_string()), 1_100, true, Some("c"));
    assert_eq!(state.active_owner.as_deref(), Some("a"));
    assert_eq!(state.lease_until, 1_000);

    adopt_owner_view(&mut state, Some("b".to_string()), 1_100, true, Some("b"));
    assert_eq!(state.active_owner.as_deref(), Some("a"));
    assert_eq!(state.lease_until, 1_000);

    adopt_owner_view(&mut state, None, 0, true, None);
    assert_eq!(state.active_owner.as_deref(), Some("a"));

    adopt_owner_view(&mut state, Some("b".to_string()), 1_500, true, None);
    assert_eq!(state.lease_until, 1_000);

    state.active_owner = None;
    adopt_owner_view(&mut state, Some("b".to_string()), 1_100, true, Some("b"));
    assert_eq!(state.active_owner.as_deref(), Some("b"));
}

#[test]
fn adopt_owner_view_accepts_first_owner_opinion() {
    let mut state = ClusterState::default();
    let mut a = empty_node("a", "a", "http://a", 1, false, now_secs());
    a.health = ClusterHealth::healthy();
    state.nodes.insert("a".to_string(), a);
    adopt_owner_view(&mut state, Some("a".to_string()), 42, false, Some("a"));
    assert_eq!(state.active_owner.as_deref(), Some("a"));
    assert_eq!(state.lease_until, 42);
}

#[test]
fn peer_view_restores_restarted_local_owner() {
    let now = now_secs();
    let mut state = ClusterState::default();
    let mut local = empty_node("a", "a", "http://a", 1, true, now);
    local.health = ClusterHealth::healthy();
    let mut peer = empty_node("b", "b", "http://b", 1, false, now);
    peer.health = ClusterHealth::healthy();
    state.nodes.insert("a".to_string(), local);
    state.nodes.insert("b".to_string(), peer);

    adopt_owner_view(&mut state, Some("a".to_string()), 42, false, Some("b"));

    assert_eq!(state.active_owner.as_deref(), Some("a"));
    assert_eq!(state.lease_until, 42);
}

#[test]
fn peer_view_does_not_restore_unhealthy_local_owner() {
    let now = now_secs();
    let mut state = ClusterState::default();
    let local = empty_node("a", "a", "http://a", 1, true, now);
    let mut peer = empty_node("b", "b", "http://b", 1, false, now);
    peer.health = ClusterHealth::healthy();
    state.nodes.insert("a".to_string(), local);
    state.nodes.insert("b".to_string(), peer);

    adopt_owner_view(&mut state, Some("a".to_string()), 42, false, Some("b"));

    assert_eq!(state.active_owner, None);
}

#[test]
fn manual_mode_peer_owner_view_does_not_replace_existing_owner() {
    let mut state = ClusterState {
        active_owner: Some("a".to_string()),
        lease_until: 1_000,
        ..ClusterState::default()
    };

    adopt_owner_view(&mut state, Some("b".to_string()), 1_100, false, None);

    assert_eq!(state.active_owner.as_deref(), Some("a"));
    assert_eq!(state.lease_until, 1_000);
}

#[test]
fn manual_mode_accepts_direct_active_owner_view_after_recovery() {
    let mut state = ClusterState {
        active_owner: Some("a".to_string()),
        forced_owner: Some("a".to_string()),
        lease_until: 1_000,
        ..ClusterState::default()
    };
    let mut b = empty_node("b", "b", "http://b", 1, false, now_secs());
    b.health = ClusterHealth::healthy();
    state.nodes.insert("b".to_string(), b);

    adopt_owner_view(&mut state, Some("b".to_string()), 1_100, false, Some("b"));

    assert_eq!(state.active_owner.as_deref(), Some("b"));
    assert_eq!(state.forced_owner, None);
    assert_eq!(state.lease_until, 1_100);
}

#[test]
fn auto_failover_disabled_does_not_pick_higher_priority_peer() {
    let mut cfg = test_config("a", 1);
    cfg.cluster.auto_failover = false;
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 100,
    }];

    let now = now_secs();
    let mut state = ClusterState {
        active_owner: Some("a".to_string()),
        ..ClusterState::default()
    };
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 1, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 100, false, now),
    );
    for node in state.nodes.values_mut() {
        node.health = ClusterHealth::healthy();
        node.last_seen = Some(now);
    }
    state.nodes.get_mut("a").unwrap().network_unstable = true;
    state.nodes.get_mut("a").unwrap().health =
        ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);

    assert_eq!(choose_owner(&state, &cfg, now), Some("a".to_string()));
}

#[test]
fn manual_mode_keeps_unhealthy_remote_owner() {
    let mut cfg = test_config("a", 100);
    cfg.cluster.auto_failover = false;
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 1,
    }];

    let now = now_secs();
    let mut state = ClusterState {
        active_owner: Some("b".to_string()),
        ..ClusterState::default()
    };
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 100, true, now),
    );
    state.nodes.insert(
        "b".to_string(),
        empty_node("b", "b", "http://b", 1, false, now),
    );
    state.nodes.get_mut("a").unwrap().health = ClusterHealth::healthy();
    state.nodes.get_mut("a").unwrap().last_seen = Some(now);
    state.nodes.get_mut("b").unwrap().network_unstable = true;
    state.nodes.get_mut("b").unwrap().health =
        ClusterHealth::unhealthy("ffmpeg_repeated_failures", false, true);
    state.nodes.get_mut("b").unwrap().last_seen = Some(now);

    assert_eq!(choose_owner(&state, &cfg, now), Some("b".to_string()));
}

#[test]
fn manual_mode_standby_without_toggles_does_not_self_elect() {
    let mut cfg = test_config("a", 100);
    cfg.cluster.auto_failover = false;
    cfg.bililive.enable_danmaku_command = false;
    cfg.enable_youtube_monitor = false;
    cfg.enable_twitch_monitor = false;
    cfg.youtube.enable_monitor = false;
    cfg.twitch.enable_monitor = false;
    cfg.priority_channel.enabled = false;
    cfg.priority_channel.auto_restart = false;
    cfg.cluster.peers = vec![crate::config::ClusterPeer {
        node_id: "b".to_string(),
        name: "b".to_string(),
        api_url: "http://b".to_string(),
        priority: 1,
    }];

    let now = now_secs();
    let mut state = ClusterState::default();
    state.nodes.insert(
        "a".to_string(),
        empty_node("a", "a", "http://a", 100, true, now),
    );
    state.nodes.get_mut("a").unwrap().health = ClusterHealth::healthy();
    state.nodes.get_mut("a").unwrap().last_seen = Some(now);

    assert_eq!(choose_owner(&state, &cfg, now), None);
}
