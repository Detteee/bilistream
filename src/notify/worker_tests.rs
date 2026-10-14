//! Worker tests: binding through a mocked gateway event, sender gating.

use super::*;
use crate::notify::observer::NodeSlot;
use crate::notify::test_support::MockQq;
use serde_json::json;

const OPENID: &str = "E4F0A1B2C3D4E5F60718293A4B5C6D7E";

struct Temp(std::path::PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn store() -> (Temp, Arc<Store>) {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let root = std::env::temp_dir().join(format!(
        "bilistream-notify-worker-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    std::fs::create_dir_all(root.join("legacy")).unwrap();
    let store = Store::open(
        root.join("data"),
        root.join("keys/master.key"),
        Some(root.join("legacy")),
    )
    .unwrap();
    (Temp(root), store)
}

fn configure(store: &Store, mock: &MockQq, openid: &str) {
    let mut cfg = NotifyConfig {
        enabled: true,
        ..Default::default()
    };
    cfg.qq.app_id = "1234567890".into();
    cfg.qq.api_base = mock.base.clone();
    cfg.qq.token_url = mock.token_url();
    cfg.qq.keep_online = false;
    store::save_config(store, &cfg, None).unwrap();
    store::save_secret(store, Some("synthetic-secret"), None).unwrap();
    if !openid.is_empty() {
        set_openid(store, openid);
    }
}

fn set_openid(store: &Store, openid: &str) {
    store::write_openid(
        store,
        OpenidWrite {
            change: OpenidChange::Set {
                value: zeroize::Zeroizing::new(openid.to_string()),
                via: "manual",
            },
            expected: None,
            writer: "test".into(),
            now: 1,
        },
    )
    .unwrap();
}

fn stored_openid(store: &Store) -> Option<String> {
    store::load_openid(store)
        .unwrap()
        .openid
        .map(|id| id.expose().to_string())
}

fn single_node() -> ClusterDigest {
    ClusterDigest {
        enabled: false,
        now: 0,
        local: "solo".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn bind_by_code_saves_openid_and_replies_passively() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, "");
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    let view = worker.start_bind(BindMode::Code, false, now).unwrap();
    assert_eq!(view["state"], "waiting");
    let code = view["code"].as_str().unwrap().to_string();

    // A message without the code is ignored and gets no reply.
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: OPENID.into(),
            msg_id: "m-0".into(),
            content: "hi".into(),
            at: now,
        })
        .await;
    assert!(mock.messages().is_empty());
    assert_eq!(stored_openid(&store), None);

    // The mocked gateway delivers the code.
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: OPENID.into(),
            msg_id: "m-1".into(),
            content: format!("绑定 {code}"),
            at: now + 1,
        })
        .await;
    // Sealed in notify-openid, never in notify-config.
    assert_eq!(stored_openid(&store).as_deref(), Some(OPENID));
    let state = store::load_openid(&store).unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(state.via.as_deref(), Some("message"));
    let config = store.read(store::CONFIG_RECORD).unwrap().unwrap();
    assert!(!config.value.to_string().contains(OPENID));
    let messages = mock.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, OPENID);
    assert_eq!(
        messages[0].1,
        json!({"msg_type": 0, "content": "绑定成功", "msg_id": "m-1", "msg_seq": 1})
    );
    assert_eq!(worker.binder.view(now + 2, "solo")["state"], "bound");
    assert!(worker.capable());
    // Neither the bind view nor the status carries the value.
    let status = worker.status(now + 2, &single_node());
    assert_eq!(status["bind"]["state"], "bound");
    assert_eq!(status["openid_set"], true);
    assert_eq!(status["openid_revision"], 1);
    assert!(!status.to_string().contains(OPENID), "{status}");

    // Window closed: later messages bind nothing and get no reply.
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: "OTHER000000000000000000000000000".into(),
            msg_id: "m-2".into(),
            content: code,
            at: now + 3,
        })
        .await;
    assert_eq!(mock.messages().len(), 1);
    assert_eq!(stored_openid(&store).as_deref(), Some(OPENID));
}

#[tokio::test]
async fn friend_add_binds_only_in_any_mode() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, "");
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    // Code mode: a FRIEND_ADD carries no code and binds nothing.
    worker.start_bind(BindMode::Code, false, now).unwrap();
    worker
        .handle_gateway_event(GatewayEvent::FriendAdd {
            openid: OPENID.into(),
            event_id: Some("FRIEND_ADD:0".into()),
            at: now,
        })
        .await;
    assert_eq!(stored_openid(&store), None);
    assert!(mock.messages().is_empty());
    worker.start_bind(BindMode::Any, false, now).unwrap();
    worker
        .handle_gateway_event(GatewayEvent::FriendAdd {
            openid: OPENID.into(),
            event_id: Some("FRIEND_ADD:1".into()),
            at: now,
        })
        .await;
    assert_eq!(stored_openid(&store).as_deref(), Some(OPENID));
    assert_eq!(mock.messages()[0].1["event_id"], "FRIEND_ADD:1");
}

#[tokio::test]
async fn messages_outside_a_window_never_get_a_reply() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    for (openid, msg) in [(OPENID, "a-1"), ("STRANGER00000000000000000000000", "s-1")] {
        worker
            .handle_gateway_event(GatewayEvent::C2c {
                openid: openid.into(),
                msg_id: msg.into(),
                content: "hi".into(),
                at: now,
            })
            .await;
    }
    assert!(mock.messages().is_empty());
    // The admin's message opened a passive window and a wakeup cycle.
    assert_eq!(
        worker.passive.as_ref().map(|p| p.msg_id.as_str()),
        Some("a-1")
    );
    assert_eq!(worker.ledger.wakeup.admin_last_c2c_at, Some(now));
}

#[tokio::test]
async fn bind_on_a_standby_node_points_to_the_owner() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, "");
    let mut worker = Worker::new(Arc::clone(&store), "b".into());
    let digest = ClusterDigest {
        enabled: true,
        local: "b".into(),
        owner: Some("a".into()),
        quorum: true,
        fence_block: Some("本节点已不是集群活跃节点".into()),
        peer_count: 1,
        nodes: vec![
            NodeSlot {
                id: "a".into(),
                healthy: true,
                fresh: true,
                ..Default::default()
            },
            NodeSlot {
                id: "b".into(),
                healthy: true,
                fresh: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let now = now_secs();
    worker.apply_digest(&digest, false, now);
    let refusal = worker.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.code(), "not_owner");
    assert_eq!(refusal.data, json!({"code": "not_owner", "owner": "a"}));
    assert!(refusal.message.contains('a'));
    assert_eq!(
        worker.writer,
        Some(NodeRef {
            node_id: "a".into(),
            is_local: false
        })
    );
}

#[tokio::test]
async fn bind_needs_saved_credentials() {
    let (_temp, store) = store();
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let now = now_secs();
    // Single node, nothing configured: still the writer (O2), so the
    // refusal is about the credentials, not about the writer.
    worker.apply_digest(&single_node(), false, now);
    assert!(worker.writer.as_ref().is_some_and(|writer| writer.is_local));
    let refusal = worker.start_bind(BindMode::Any, false, now).unwrap_err();
    assert_eq!(refusal.code(), "credentials_missing");
    assert_eq!(refusal.data, json!({"code": "credentials_missing"}));
    assert!(refusal.message.contains("AppSecret"));
}

#[tokio::test]
async fn bind_refusal_codes_cover_no_writer_and_bad_credentials() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, "");
    let mut worker = Worker::new(Arc::clone(&store), "b".into());
    let now = now_secs();
    // No quorum: no writer anywhere, even the no-quorum fallback (O4).
    let mut digest = ClusterDigest {
        enabled: true,
        local: "b".into(),
        owner: Some("a".into()),
        quorum: false,
        fence_block: Some("集群无多数派".into()),
        peer_count: 1,
        nodes: vec![
            slot("a", false, "heartbeat_timeout"),
            slot("b", true, "healthy"),
        ],
        ..Default::default()
    };
    digest.nodes[0].fresh = false;
    worker.apply_digest(&digest, false, now);
    assert_eq!(worker.writer, None);
    let refusal = worker.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.data, json!({"code": "no_writer"}));
    // Writer is local, but rejected credentials: bad_credentials.
    worker.apply_digest(&single_node(), false, now);
    worker.bad_credentials = true;
    let refusal = worker.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.data, json!({"code": "bad_credentials"}));
}

#[tokio::test]
async fn bind_refuses_with_no_writer_while_the_writer_is_not_the_gateway_holder() {
    // O6: the writer and the gateway holder differ (here: this node is the
    // fence primary but another session is still inside its hysteresis
    // window, modelled by `gateway_owner == false`): no second session.
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, "");
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    worker.gateway_owner = false;
    let refusal = worker.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.data, json!({"code": "no_writer"}));
    assert!(worker.binder.view(now, "solo")["state"] == "idle");
    worker.manage_gateway(now);
    assert!(worker.gateway.is_none());
}

#[tokio::test]
async fn bind_with_keep_online_off_opens_the_gateway_on_the_writer_only_for_the_window() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, ""); // keep_online: false
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    worker.manage_gateway(now);
    assert!(worker.gateway.is_none(), "keep_online off: no session");
    worker.start_bind(BindMode::Code, false, now).unwrap();
    worker.manage_gateway(now);
    assert!(worker.gateway.is_some(), "bind window opens the gateway");
    worker.binder.cancel();
    worker.manage_gateway(now + 1);
    assert!(worker.gateway.is_none(), "closed when the window ends");
}

#[tokio::test]
async fn test_send_reports_the_sending_node() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let report = worker.test_send(now_secs()).await;
    assert!(report.ok, "{report:?}");
    assert_eq!(report.node_id, "solo");
    assert_eq!(report.mode.as_deref(), Some("active"));
    let body = &mock.messages()[0].1;
    assert!(body["content"].as_str().unwrap().contains("通知测试"));
    assert!(!body.to_string().contains("synthetic-secret"));
}

fn slot(id: &str, healthy: bool, reason: &str) -> NodeSlot {
    NodeSlot {
        id: id.into(),
        healthy,
        reason: reason.into(),
        stale: !healthy,
        fresh: healthy,
        notify_capable: true,
        ..Default::default()
    }
}

fn cluster(local: &str, owner: &str, nodes: Vec<NodeSlot>) -> ClusterDigest {
    let fence = (local != owner).then(|| "本节点已不是集群活跃节点".to_string());
    ClusterDigest {
        enabled: true,
        local: local.into(),
        owner: Some(owner.into()),
        quorum: true,
        fence_block: fence,
        peer_count: nodes.len() - 1,
        members: nodes.iter().map(|n| n.id.clone()).collect(),
        nodes,
        ..Default::default()
    }
}

async fn ready_worker(store: &Arc<Store>, node: &str) -> Worker {
    let mut worker = Worker::new(Arc::clone(store), node.into());
    worker.started_at = 0;
    worker
}

#[tokio::test]
async fn failover_survives_the_handoff_hold_on_the_new_owner() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "b").await;
    let now = now_secs();
    let healthy = || {
        vec![
            slot("a", true, "healthy"),
            slot("b", true, "healthy"),
            slot("c", true, "healthy"),
        ]
    };
    worker.apply_digest(&cluster("b", "a", healthy()), false, now);
    // a dies; b takes over with a handoff hold (Freeze) in the same write.
    let dead = || {
        vec![
            slot("a", false, "heartbeat_timeout"),
            slot("b", true, "healthy"),
            slot("c", true, "healthy"),
        ]
    };
    let mut held = cluster("b", "b", dead());
    held.handoff_pending = true;
    held.fence_block = Some("集群节点交接尚未确认".into());
    worker.apply_digest(&held, false, now + 5);
    assert!(worker
        .tracker
        .queue()
        .iter()
        .all(|i| i.kind != EventKind::Failover));
    // The hold clears: exactly one failover, old → new.
    worker.apply_digest(&cluster("b", "b", dead()), false, now + 20);
    worker.apply_digest(&cluster("b", "b", dead()), false, now + 25);
    let failovers: Vec<_> = worker
        .tracker
        .queue()
        .iter()
        .filter(|i| i.kind == EventKind::Failover)
        .collect();
    assert_eq!(failovers.len(), 1);
    assert_eq!(failovers[0].node, "b");
    assert_eq!(failovers[0].extra.old_owner.as_deref(), Some("a"));
    assert_eq!(worker.authority, Authority::Primary);
}

#[tokio::test]
async fn removed_members_get_no_false_recovery() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "b").await;
    let now = now_secs();
    let nodes = || {
        vec![
            slot("a", false, "heartbeat_timeout"),
            slot("b", true, "healthy"),
            slot("c", true, "healthy"),
        ]
    };
    worker.ever_healthy.insert("a".into());
    worker.apply_digest(&cluster("b", "b", nodes()), false, now);
    worker.apply_digest(&cluster("b", "b", nodes()), false, now + 120);
    assert!(worker
        .tracker
        .queue()
        .iter()
        .any(|i| i.kind == EventKind::NodeDown));
    let queued = worker.tracker.queue().to_vec();
    worker.tracker.delivered(&queued, now + 121);
    worker.tracker.discard(|_| true);
    // a is removed through membership: it vanishes from nodes and members.
    let after = cluster(
        "b",
        "b",
        vec![slot("b", true, "healthy"), slot("c", true, "healthy")],
    );
    for t in [200, 400, 800] {
        worker.apply_digest(&after, false, now + t);
    }
    assert!(
        worker.tracker.queue().is_empty(),
        "{:?}",
        worker.tracker.queue()
    );
    assert_eq!(worker.tracker.open_incidents(), 0);
}

#[tokio::test]
async fn a_delivered_undelivered_summary_is_marked_reported() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker
        .ledger
        .record_undelivered(2, "主动消息无权限", now - 100);
    worker.apply_digest(&single_node(), false, now);
    worker.tracker.on_event(
        AlertEvent::new(EventKind::Failover, Phase::Fire, "b", now - 30).with_extra(ItemExtra {
            old_owner: Some("a".into()),
            live: None,
        }),
        &worker.policy,
    );
    worker.deliver(now, &single_node()).await;
    let content = mock.messages()[0].1["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(content.contains("另有 2 条通知未送达"), "{content}");
    assert_eq!(worker.ledger.undelivered_pending(), 0);
}

#[tokio::test]
async fn undeliverable_resolved_items_wait_and_ride_along() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    let resolved = OutItem {
        key: "cluster.node_down:a".into(),
        kind: EventKind::NodeDown,
        item: ItemKind::Resolved { duration_secs: 480 },
        node: "a".into(),
        detail: None,
        incident_at: now - 600,
        queued_at: now - 100,
        not_before: now - 100,
        suppressed: 0,
        flapping: None,
        extra: ItemExtra::default(),
    };
    worker.tracker.requeue(vec![resolved]);
    // Active messages refused, no passive window, not critical: undeliverable.
    mock.script([(400, json!({"code": 40034105, "message": "no active"}))]);
    worker.deliver(now, &single_node()).await;
    assert_eq!(worker.held.len(), 1);
    assert_eq!(
        worker.ledger.undelivered_pending(),
        0,
        "resolved is not 未送达"
    );
    // The admin writes; the next alert goes out passively with the held line.
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: OPENID.into(),
            msg_id: "m-9".into(),
            content: "hi".into(),
            at: now + 1,
        })
        .await;
    worker.tracker.on_event(
        AlertEvent::new(EventKind::Failover, Phase::Fire, "b", now - 30).with_extra(ItemExtra {
            old_owner: Some("a".into()),
            live: None,
        }),
        &worker.policy,
    );
    worker.next_send_at = 0;
    worker.deliver(now + 2, &single_node()).await;
    let last = mock.messages().last().unwrap().1.clone();
    assert_eq!(last["msg_id"], "m-9");
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .contains("· ✅ 节点故障 a 已恢复（8 分钟"),
        "{last}"
    );
    assert!(worker.held.is_empty());
}

#[tokio::test]
async fn admin_commands_get_a_passive_answer_and_strangers_nothing() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    let before = worker.ledger.clone();
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: "STRANGER0000000000000000000000000".into(),
            msg_id: "s-1".into(),
            content: "status".into(),
            at: now,
        })
        .await;
    assert!(mock.messages().is_empty());
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: OPENID.into(),
            msg_id: "a-1".into(),
            content: "quota".into(),
            at: now,
        })
        .await;
    let messages = mock.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].1["msg_id"], "a-1");
    assert_eq!(messages[0].1["msg_seq"], 1);
    assert!(messages[0].1["content"]
        .as_str()
        .unwrap()
        .contains("通知额度"));
    // Not a command: no reply. Too fast: no reply.
    for (id, text) in [("a-2", "hi"), ("a-3", "help")] {
        worker
            .handle_gateway_event(GatewayEvent::C2c {
                openid: OPENID.into(),
                msg_id: id.into(),
                content: text.into(),
                at: now,
            })
            .await;
    }
    assert_eq!(mock.messages().len(), 1);
    // Only the admin-message bookkeeping changed, nothing else.
    let mut expected = before.clone();
    expected.record_admin_message(now);
    assert_eq!(worker.ledger.wakeup, expected.wakeup);
    assert_eq!(worker.ledger.incidents, before.incidents);

    // Turned off in the panel: ignored.
    let (mut cfg, rev) = store::load_config(&store).unwrap();
    cfg.commands_enabled = false;
    store::save_config(&store, &cfg, rev).unwrap();
    worker.reload_settings();
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: OPENID.into(),
            msg_id: "a-4".into(),
            content: "status".into(),
            at: now + 10,
        })
        .await;
    assert_eq!(mock.messages().len(), 1);
}

#[tokio::test]
async fn watched_channel_going_live_sends_one_green_notice() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let (mut cfg, rev) = store::load_config(&store).unwrap();
    cfg.watch.enabled = true;
    cfg.watch
        .channels
        .push(crate::notify::config::WatchChannel {
            platform: "youtube".into(),
            channel_id: "UCwatched".into(),
            name: "主频道".into(),
        });
    store::save_config(&store, &cfg, rev).unwrap();
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    let seen = vec![crate::notify::live::LiveObservation {
        platform: "youtube".into(),
        channel_id: "UCwatched".into(),
        title: Some("【歌枠】".into()),
    }];
    worker.channel_live(now, &seen);
    worker.deliver(now + 11, &single_node()).await;
    let messages = mock.messages();
    assert_eq!(messages.len(), 1);
    let content = messages[0].1["content"].as_str().unwrap();
    assert!(content.starts_with("🟢 开播 · 主频道\n"), "{content}");
    assert!(content.contains("平台 YouTube"));
    assert!(messages[0].1.get("is_wakeup").is_none());
    // Still live on the next wakes: no second notice.
    for t in [30, 60, 90] {
        worker.channel_live(now + t, &seen);
        worker.deliver(now + t + 11, &single_node()).await;
    }
    assert_eq!(mock.messages().len(), 1);
}

#[tokio::test]
async fn sticky_mode_facts_survive_a_restart_but_not_a_new_target() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let now = now_secs();
    {
        let mut worker = ready_worker(&store, "solo").await;
        worker.ledger.modes.active_unavailable_at = now;
        worker.save_state(now);
        if let Some(task) = worker.save_task.take() {
            task.await.unwrap();
        }
    }
    let worker = ready_worker(&store, "solo").await;
    assert!(
        worker.ledger.modes.active_unavailable(now + 1),
        "restart kept the fact"
    );
    drop(worker);
    set_openid(&store, "NEWADMIN0000000000000000000000000");
    let worker = ready_worker(&store, "solo").await;
    assert!(
        !worker.ledger.modes.active_unavailable(now + 1),
        "new admin resets it"
    );
}

#[tokio::test]
async fn non_admin_active_message_toggles_are_ignored() {
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    worker
        .handle_gateway_event(GatewayEvent::ActiveMessages {
            openid: Some("STRANGER".into()),
            accepted: false,
            at: 10,
        })
        .await;
    assert!(!worker.ledger.modes.admin_rejected());
    worker
        .handle_gateway_event(GatewayEvent::ActiveMessages {
            openid: Some(OPENID.into()),
            accepted: false,
            at: 10,
        })
        .await;
    assert!(worker.ledger.modes.admin_rejected());
}

#[tokio::test]
async fn bind_does_not_replace_an_openid_without_confirmation() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    let refusal = worker.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.data, json!({"code": "already_bound"}));
    assert!(refusal.message.contains("确认"));
    assert!(worker.start_bind(BindMode::Code, true, now).is_ok());
}

#[tokio::test]
async fn gateway_survives_a_short_sender_flap() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let (mut cfg, rev) = store::load_config(&store).unwrap();
    cfg.qq.keep_online = true;
    store::save_config(&store, &cfg, rev).unwrap();
    let mut worker = ready_worker(&store, "a").await;
    worker.reload_settings();
    let now = now_secs();
    let nodes = || vec![slot("a", true, "healthy"), slot("b", true, "healthy")];
    worker.apply_digest(&cluster("a", "a", nodes()), false, now);
    worker.manage_gateway(now);
    assert!(worker.gateway.is_some());
    // A momentary fence block: the session stays for the hysteresis window.
    worker.apply_digest(&cluster("a", "b", nodes()), false, now + 1);
    worker.manage_gateway(now + 1);
    assert!(worker.gateway.is_some());
    worker.apply_digest(&cluster("a", "b", nodes()), false, now + 31);
    worker.manage_gateway(now + 31);
    assert!(worker.gateway.is_none());
    // A fatal close stops it for good until the settings change.
    worker.apply_digest(&cluster("a", "a", nodes()), false, now + 40);
    worker
        .handle_gateway_event(GatewayEvent::Closed {
            reason: "机器人已被封禁 (4915)".into(),
            fatal: true,
            bad_credentials: false,
        })
        .await;
    worker.manage_gateway(now + 41);
    assert!(worker.gateway.is_none());
    assert_eq!(worker.gateway_state.0, "failed");
}

#[tokio::test]
async fn supervisor_restarts_a_panicked_worker_and_keeps_the_channel() {
    let (sender, receiver) = mpsc::channel(4);
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&runs);
    let task = tokio::spawn(supervise_loop(
        receiver,
        Duration::from_millis(10),
        move |commands| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                let run = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if run == 0 {
                    panic!("injected worker panic");
                }
                // The second run still receives commands sent after the panic.
                while let Some(command) = commands.recv().await {
                    if let Command::Test { reply } = command {
                        let _ = reply.send(TestReport {
                            ok: true,
                            node_id: format!("run-{run}"),
                            mode: None,
                            code: None,
                            reason: None,
                            trace_id: None,
                        });
                    }
                }
                true
            })
        },
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    sender.send(Command::Test { reply: tx }).await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.node_id, "run-1");
    drop(sender);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn wrong_secret_stops_reconnects_until_the_secret_is_saved_again() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let (mut cfg, rev) = store::load_config(&store).unwrap();
    cfg.qq.keep_online = true;
    store::save_config(&store, &cfg, rev).unwrap();
    mock.state.lock().unwrap().token_reply = Some((
        200,
        json!({"code": 100016, "message": "invalid appid or secret"}),
    ));
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    worker.manage_gateway(now);
    assert!(worker.gateway.is_some());
    let event = tokio::time::timeout(Duration::from_secs(5), worker.gateway_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            event,
            GatewayEvent::Closed {
                bad_credentials: true,
                ..
            }
        ),
        "{event:?}"
    );
    worker.handle_gateway_event(event).await;
    assert!(worker.bad_credentials);
    let calls = mock.state.lock().unwrap().token_calls;
    assert_eq!(calls, 1);
    // No reconnect, no send attempt, no bind while flagged.
    for t in 1..5 {
        worker.apply_digest(&single_node(), false, now + t);
        worker.manage_gateway(now + t);
        assert!(worker.gateway.is_none());
    }
    worker.deliver(now + 6, &single_node()).await;
    assert_eq!(mock.state.lock().unwrap().token_calls, calls);
    let status = worker.status(now + 6, &single_node());
    assert_eq!(status["bad_credentials"], true);
    assert_eq!(status["gateway"]["state"], "failed");
    // The user saves the (corrected) secret: the flag clears, one reconnect.
    mock.state.lock().unwrap().token_reply = None;
    let (_, secret_rev) = store::load_secret(&store).unwrap();
    store::save_secret(&store, Some("synthetic-secret-2"), secret_rev).unwrap();
    worker.reload_settings();
    assert!(!worker.bad_credentials);
    worker.apply_digest(&single_node(), false, now + 7);
    worker.manage_gateway(now + 7);
    assert!(worker.gateway.is_some(), "saving the secret reconnects");
    tokio::time::timeout(Duration::from_secs(5), async {
        while mock.state.lock().unwrap().token_calls == calls {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a fresh token fetch with the new secret");
}

#[tokio::test]
async fn a_bad_credential_send_keeps_the_gateway_down() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let (mut cfg, rev) = store::load_config(&store).unwrap();
    cfg.qq.keep_online = true;
    store::save_config(&store, &cfg, rev).unwrap();
    mock.state.lock().unwrap().token_reply = Some((200, json!({"code": 100007})));
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    let report = worker.test_send(now).await;
    assert!(!report.ok);
    assert!(worker.bad_credentials);
    worker.manage_gateway(now + 1);
    assert!(
        worker.gateway.is_none(),
        "M1: no respawn after a bad-credential send"
    );
    // A READY (from a session that was already up) clears the flag.
    worker.handle_gateway_event(GatewayEvent::Ready).await;
    assert!(!worker.bad_credentials);
}

/// Clerk's 1a gate: after failover to a node without the openid (and no
/// other fully configured node), an alert is counted undelivered and the
/// status shows a warning; never a silent drop.
#[tokio::test]
async fn failover_to_a_node_without_the_openid_counts_undelivered_with_a_warning() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, ""); // AppSecret yes, openid no (1a: no copy)
    let mut worker = ready_worker(&store, "b").await;
    let now = now_secs();
    let nodes = |a_alive: bool, c_capable: bool| {
        let mut nodes = vec![
            slot(
                "a",
                a_alive,
                if a_alive {
                    "healthy"
                } else {
                    "heartbeat_timeout"
                },
            ),
            slot("b", true, "healthy"),
            slot("c", true, "healthy"),
        ];
        nodes[1].notify_capable = false;
        nodes[2].notify_capable = c_capable;
        nodes
    };
    worker.apply_digest(&cluster("b", "a", nodes(true, false)), false, now);
    // a (the only configured node) dies; b becomes the owner.
    let dead = cluster("b", "b", nodes(false, false));
    worker.apply_digest(&dead, false, now + 20);
    worker.apply_digest(&dead, false, now + 25);
    assert_eq!(worker.authority, Authority::None);
    assert!(worker.writer.as_ref().is_some_and(|w| w.is_local));
    assert!(worker.tracker.queued() > 0, "the failover is queued");
    let queued = worker
        .tracker
        .queue()
        .iter()
        .filter(|item| item.item == ItemKind::Alert)
        .count() as u32;
    worker.deliver(now + 60, &dead).await;
    assert!(mock.messages().is_empty());
    assert_eq!(worker.tracker.queued(), 0);
    assert_eq!(worker.ledger.undelivered_pending(), queued);
    let status = worker.status(now + 60, &dead);
    assert_eq!(status["warning"], WARNING_NO_CAPABLE_NODE);
    assert_eq!(status["undelivered"], queued);
    assert_eq!(status["capable"], false);
    assert_eq!(status["openid_set"], false);
    assert!(status["last_reason"]["reason"]
        .as_str()
        .unwrap()
        .contains("未配置完整"));

    // With a fully configured peer the delegate sends: b counts nothing.
    let (_temp2, store2) = self::store();
    configure(&store2, &mock, "");
    let mut other = ready_worker(&store2, "b").await;
    other.apply_digest(&cluster("b", "a", nodes(true, true)), false, now);
    let delegated = cluster("b", "b", nodes(false, true));
    other.apply_digest(&delegated, false, now + 20);
    other.apply_digest(&delegated, false, now + 25);
    let before = other.tracker.queued();
    other.deliver(now + 60, &delegated).await;
    assert_eq!(other.tracker.queued(), before);
    assert_eq!(other.ledger.undelivered_pending(), 0);
    assert!(other.status(now + 60, &delegated)["warning"].is_null());
}

#[tokio::test]
async fn real_sends_reach_the_heartbeat_ledger_without_reasons_and_reasons_are_masked() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    // A test send is local only.
    assert!(worker.test_send(now).await.ok);
    assert!(worker.ledger.last_send.is_empty());
    worker.outcome(
        false,
        now + 1,
        Some("active"),
        Some(11255),
        Some(format!("QQ 接口返回 11255：invalid openid={OPENID}")),
        Some("0123456789abcdef0123456789abcdef".into()),
        false,
    );
    let sent = &worker.ledger.last_send["solo"];
    assert_eq!((sent.at, sent.ok, sent.code), (now + 1, false, Some(11255)));
    assert_eq!(sent.via.as_deref(), Some("active"));
    let heartbeat = serde_json::to_string(&worker.ledger).unwrap();
    assert!(
        !heartbeat.contains("11255："),
        "no reason text in the heartbeat"
    );
    assert!(
        !heartbeat.contains("0123456789abcdef"),
        "no trace id in the heartbeat"
    );
    let status = worker.status(now + 1, &single_node());
    assert_eq!(
        status["local_last_send"]["reason"],
        "QQ 接口返回 11255：invalid openid=<set>"
    );
    assert_eq!(status["last_outcome"], status["local_last_send"]);
    assert!(!status.to_string().contains(OPENID));
    // A gateway close reason echoing the openid is masked as well.
    worker
        .handle_gateway_event(GatewayEvent::Closed {
            reason: format!("closed for {OPENID}"),
            fatal: false,
            bad_credentials: false,
        })
        .await;
    let status = worker.status(now + 2, &single_node());
    assert_eq!(status["gateway"]["reason"], "closed for <set>");
}

#[tokio::test]
async fn unbinding_returns_a_bound_view_to_idle_and_drops_capability() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, "");
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    worker.start_bind(BindMode::Any, false, now).unwrap();
    worker
        .handle_gateway_event(GatewayEvent::FriendAdd {
            openid: OPENID.into(),
            event_id: Some("FRIEND_ADD:1".into()),
            at: now,
        })
        .await;
    assert!(worker.capable());
    let revision = store::load_openid(&store).unwrap().revision;
    store::write_openid(
        &store,
        OpenidWrite {
            change: OpenidChange::Clear,
            expected: Some(revision),
            writer: "solo".into(),
            now,
        },
    )
    .unwrap();
    worker.reload_settings();
    assert!(!worker.capable());
    assert_eq!(worker.binder.view(now, "solo")["state"], "idle");
    let status = worker.status(now, &single_node());
    assert_eq!(status["openid_set"], false);
    assert_eq!(status["openid_revision"], revision + 1);
    assert!(status["openid_bound_at"].is_null());
}

// ---------------------------------------------------------------------------
// Writer = sender-authority node (contract, Architect 17:43). Tests (a)–(e).

fn three_nodes_without_openid() -> Vec<NodeSlot> {
    ["a", "b", "c"]
        .into_iter()
        .map(|id| NodeSlot {
            notify_capable: false,
            ..slot(id, true, "healthy")
        })
        .collect()
}

/// Same rule the PUT/DELETE handlers apply (`notify::check_writer`).
fn put_allowed(worker: &Worker) -> Result<(), Refusal> {
    crate::notify::writer_allows(worker.writer.as_ref(), "修改管理员 openid")
}

/// (a) A fresh 3-node cluster with no openid anywhere: the authority node
/// accepts bind and PUT; the others answer not_owner.
#[tokio::test]
async fn fresh_cluster_without_any_openid_binds_on_the_authority_node() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let mut workers = Vec::new();
    let mut temps = Vec::new();
    for id in ["a", "b", "c"] {
        let (temp, store) = store();
        configure(&store, &mock, "");
        let mut worker = ready_worker(&store, id).await;
        let now = now_secs();
        worker.apply_digest(&cluster(id, "a", three_nodes_without_openid()), false, now);
        assert_eq!(worker.authority, Authority::None, "nobody can send yet");
        temps.push(temp);
        workers.push(worker);
    }
    let now = now_secs();
    let a = &mut workers[0];
    assert_eq!(
        a.writer,
        Some(NodeRef {
            node_id: "a".into(),
            is_local: true
        })
    );
    put_allowed(a).unwrap();
    let view = a.start_bind(BindMode::Code, false, now).unwrap();
    assert_eq!(view["state"], "waiting");
    let status = a.status(now, &cluster("a", "a", three_nodes_without_openid()));
    assert_eq!(
        status["openid_writer"],
        json!({"node_id": "a", "is_local": true})
    );
    for worker in &mut workers[1..] {
        let expected = json!({"code": "not_owner", "owner": "a"});
        assert_eq!(put_allowed(worker).unwrap_err().data, expected);
        assert_eq!(
            worker
                .start_bind(BindMode::Code, false, now)
                .unwrap_err()
                .data,
            expected
        );
    }
}

/// (b) keep_online off, cluster: bind on the writer opens the gateway for
/// the window and closes it when the window times out.
#[tokio::test]
async fn keep_online_off_bind_opens_and_closes_the_gateway_on_the_writer() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, ""); // keep_online: false
    let mut worker = ready_worker(&store, "a").await;
    let now = now_secs();
    let digest = cluster("a", "a", three_nodes_without_openid());
    worker.apply_digest(&digest, false, now);
    worker.manage_gateway(now);
    assert!(worker.gateway.is_none(), "no session without keep_online");
    worker.start_bind(BindMode::Code, false, now).unwrap();
    worker.manage_gateway(now);
    assert!(worker.gateway.is_some(), "the bind window opens it");
    // Window over (timeout): the next step expires it and closes the session.
    let late = now + super::super::bind::DEFAULT_WINDOW_SECS + 1;
    worker.apply_digest(&digest, false, late);
    worker.manage_gateway(late);
    assert_eq!(worker.binder.view(late, "a")["state"], "expired");
    assert!(worker.gateway.is_none(), "closed after the window");
}

/// (c) A node still holding the gateway (hysteresis) but no longer the
/// authority answers not_owner, naming the authority node.
#[tokio::test]
async fn gateway_holder_without_authority_answers_not_owner() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    // No openid anywhere, so a cannot become the delegate after the move.
    configure(&store, &mock, "");
    let (mut cfg, rev) = store::load_config(&store).unwrap();
    cfg.qq.keep_online = true;
    store::save_config(&store, &cfg, rev).unwrap();
    let mut worker = ready_worker(&store, "a").await;
    let now = now_secs();
    worker.apply_digest(&cluster("a", "a", three_nodes_without_openid()), false, now);
    worker.manage_gateway(now);
    assert!(worker.gateway.is_some());
    // Authority moved to b; a keeps its session for the hysteresis window.
    worker.apply_digest(
        &cluster("a", "b", three_nodes_without_openid()),
        false,
        now + 1,
    );
    worker.manage_gateway(now + 1);
    assert!(worker.gateway.is_some(), "still holding the gateway");
    let refusal = worker
        .start_bind(BindMode::Code, true, now + 1)
        .unwrap_err();
    assert_eq!(refusal.data, json!({"code": "not_owner", "owner": "b"}));
    assert_eq!(
        put_allowed(&worker).unwrap_err().data,
        json!({"code": "not_owner", "owner": "b"})
    );
}

/// (d) Mid-handoff: bind answers no_writer and opens nothing.
#[tokio::test]
async fn mid_handoff_bind_answers_no_writer() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_temp, store) = store();
    configure(&store, &mock, "");
    let mut worker = ready_worker(&store, "b").await;
    let now = now_secs();
    // b is the new owner but the handoff hold has not cleared yet.
    let mut held = cluster("b", "b", three_nodes_without_openid());
    held.handoff_pending = true;
    held.fence_block = Some("集群节点交接尚未确认".into());
    worker.apply_digest(&held, false, now);
    assert_eq!(worker.writer, None);
    let refusal = worker.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.data, json!({"code": "no_writer"}));
    worker.manage_gateway(now);
    assert!(worker.gateway.is_none());
    // Seen from a standby during the same handoff: no writer either.
    let mut standby = ready_worker(&store, "c").await;
    let mut held_c = cluster("c", "b", three_nodes_without_openid());
    held_c.handoff_pending = true;
    standby.apply_digest(&held_c, false, now);
    assert_eq!(standby.writer, None);
    // O6: the writer is local but another node still holds a session.
    let mut writer = ready_worker(&store, "a").await;
    writer.apply_digest(&cluster("a", "a", three_nodes_without_openid()), false, now);
    writer.gateway_holder = Some(NodeRef {
        node_id: "b".into(),
        is_local: false,
    });
    let refusal = writer.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.data, json!({"code": "no_writer"}));
    // A non-bind openid write on the writer still works without a gateway.
    put_allowed(&writer).unwrap();
}

/// (e) A single node with the cluster off is always the writer, with
/// nothing configured and without a gateway.
#[tokio::test]
async fn single_node_with_the_cluster_off_is_the_writer() {
    let (_temp, store) = store();
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    worker.apply_digest(&single_node(), false, now);
    assert_eq!(
        worker.writer,
        Some(NodeRef {
            node_id: "solo".into(),
            is_local: true
        })
    );
    assert!(worker.gateway.is_none());
    put_allowed(&worker).unwrap();
    assert_eq!(
        worker.status(now, &single_node())["openid_writer"],
        json!({"node_id": "solo", "is_local": true})
    );
    // No quorum in a cluster: null, and writes are no_writer.
    let mut lost = cluster(
        "solo",
        "solo",
        vec![
            slot("solo", true, "healthy"),
            slot("x", false, "heartbeat_timeout"),
        ],
    );
    lost.quorum = false;
    lost.fence_block = Some("集群心跳多数派已丢失".into());
    worker.apply_digest(&lost, false, now);
    assert_eq!(worker.writer, None);
    assert_eq!(
        put_allowed(&worker).unwrap_err().data,
        json!({"code": "no_writer"})
    );
}

// ---------------------------------------------------------------------------
// Writer = sender authority, including a delegate (Architect 18:12).

/// a = owner without the openid, b = fully configured (the delegate),
/// c = not configured.
fn delegate_nodes(owner_capable: bool) -> Vec<NodeSlot> {
    let mut nodes = three_nodes_without_openid();
    nodes[0].notify_capable = owner_capable;
    nodes[1].notify_capable = true;
    nodes
}

async fn worker_on(
    mock: &MockQq,
    id: &str,
    openid: &str,
    keep_online: bool,
) -> (Temp, Arc<Store>, Worker) {
    let (temp, store) = store();
    configure(&store, mock, openid);
    if keep_online {
        let (mut cfg, rev) = store::load_config(&store).unwrap();
        cfg.qq.keep_online = true;
        store::save_config(&store, &cfg, rev).unwrap();
    }
    let worker = ready_worker(&store, id).await;
    (temp, store, worker)
}

/// (a) + (b): the owner has no openid and a delegate sends. Bind works on
/// the delegate (which holds the gateway); the owner answers not_owner
/// naming the delegate; every node shows openid_writer == authority node.
#[tokio::test]
async fn while_a_delegate_sends_it_is_the_writer_and_can_bind() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let now = now_secs();
    let (_ta, _sa, mut a) = worker_on(&mock, "a", "", true).await;
    let (_tb, _sb, mut b) = worker_on(&mock, "b", OPENID, true).await;
    let (_tc, _sc, mut c) = worker_on(&mock, "c", "", true).await;
    let digest_for = |local: &str| {
        let mut digest = cluster(local, "a", delegate_nodes(false));
        // b advertises its authority in its heartbeat.
        digest.nodes[1].notify_authority = true;
        digest
    };
    a.apply_digest(&digest_for("a"), false, now);
    b.apply_digest(&digest_for("b"), false, now);
    c.apply_digest(&digest_for("c"), false, now);
    assert_eq!(b.authority, Authority::Delegate);
    assert_eq!(a.authority, Authority::None);
    for worker in [&a, &b, &c] {
        assert_eq!(
            worker.writer.as_ref().map(|w| w.node_id.as_str()),
            Some("b"),
            "on {}",
            worker.node_id
        );
    }
    // The gateway follows the sender: b holds it, the owner does not.
    a.manage_gateway(now);
    b.manage_gateway(now);
    assert!(a.gateway.is_none());
    assert!(b.gateway.is_some());
    // (a) bind on the delegate succeeds; the owner points at the delegate.
    let view = b.start_bind(BindMode::Code, true, now).unwrap();
    assert_eq!(view["state"], "waiting");
    let refusal = a.start_bind(BindMode::Code, false, now).unwrap_err();
    assert_eq!(refusal.data, json!({"code": "not_owner", "owner": "b"}));
    assert_eq!(put_allowed(&a).unwrap_err().data, refusal.data);
    put_allowed(&b).unwrap();
    // (b) /status on every node: openid_writer == authority_node_id == b.
    // The API's authority_node_id: local when this node is primary/delegate,
    // else the fresh peer advertising authority.
    for (worker, local) in [(&a, "a"), (&b, "b"), (&c, "c")] {
        let digest = digest_for(local);
        let status = worker.status(now, &digest);
        let authority_node = if matches!(worker.authority, Authority::Primary | Authority::Delegate)
        {
            Some(local.to_string())
        } else {
            digest
                .nodes
                .iter()
                .find(|node| node.id != local && node.fresh && node.notify_authority)
                .map(|node| node.id.clone())
        };
        assert_eq!(authority_node.as_deref(), Some("b"), "on {local}");
        assert_eq!(status["openid_writer"]["node_id"], "b", "on {local}");
        assert_eq!(status["openid_writer"]["is_local"], local == "b");
        assert_eq!(status["gateway_owner"]["node_id"], "b", "on {local}");
    }
}

/// (c) Once the owner is fully configured the delegate state ends and the
/// writer (and the gateway) move back to the owner.
#[tokio::test]
async fn the_writer_moves_back_to_the_owner_when_the_delegate_state_ends() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let now = now_secs();
    let (_ta, store_a, mut a) = worker_on(&mock, "a", "", false).await;
    let (_tb, _sb, mut b) = worker_on(&mock, "b", OPENID, false).await;
    a.apply_digest(&cluster("a", "a", delegate_nodes(false)), false, now);
    b.apply_digest(&cluster("b", "a", delegate_nodes(false)), false, now);
    assert_eq!(a.writer.as_ref().unwrap().node_id, "b");
    assert!(b.writer.as_ref().unwrap().is_local);
    // The owner gets its openid (bound on b in 1a means typed in on a, or
    // 1b's copy); its heartbeat now says notify_capable.
    set_openid(&store_a, OPENID);
    a.reload_settings();
    a.apply_digest(&cluster("a", "a", delegate_nodes(true)), false, now + 1);
    b.apply_digest(&cluster("b", "a", delegate_nodes(true)), false, now + 1);
    assert_eq!(a.authority, Authority::Primary);
    assert_ne!(b.authority, Authority::Delegate);
    assert!(a.writer.as_ref().unwrap().is_local);
    assert_eq!(
        b.writer,
        Some(NodeRef {
            node_id: "a".into(),
            is_local: false
        })
    );
    assert_eq!(
        b.start_bind(BindMode::Code, true, now + 1)
            .unwrap_err()
            .data,
        json!({"code": "not_owner", "owner": "a"})
    );
    assert!(a.start_bind(BindMode::Code, true, now + 1).is_ok());
}

/// The no-quorum fallback is still never a writer, even when fully configured.
#[tokio::test]
async fn the_no_quorum_fallback_is_not_a_writer() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let (_t, _s, mut worker) = worker_on(&mock, "b", OPENID, false).await;
    let now = now_secs();
    let mut lost = cluster("b", "a", delegate_nodes(false));
    lost.quorum = false;
    lost.now = now; // past the no-quorum grace since last_authority_seen = 0
    lost.nodes[0].fresh = false;
    lost.nodes[2].fresh = false;
    worker.last_authority_seen = 0;
    worker.apply_digest(&lost, false, now);
    assert_eq!(worker.authority, Authority::Fallback);
    assert_eq!(worker.writer, None);
    assert_eq!(
        worker
            .start_bind(BindMode::Code, true, now)
            .unwrap_err()
            .data,
        json!({"code": "no_writer"})
    );
}

// ---- Contract line 70: close before identify on a gateway handoff ----

/// Two nodes; `held` is what the peer's last heartbeat said.
fn pair(local: &str, owner: &str, peer_held: Option<bool>) -> ClusterDigest {
    let mut digest = cluster(
        local,
        owner,
        vec![slot("a", true, "healthy"), slot("b", true, "healthy")],
    );
    for node in &mut digest.nodes {
        if node.id != local {
            node.gateway_held = peer_held;
        }
    }
    digest
}

/// Pure rule: fresh members reporting a session block; closed, stale,
/// non-member and unknown-without-authority peers do not.
#[test]
fn may_identify_is_decided_from_heartbeats_alone() {
    use crate::notify::authority::may_identify;
    assert_eq!(may_identify(&pair("b", "b", Some(true))), Err("a".into()));
    assert_eq!(may_identify(&pair("b", "b", Some(false))), Ok(()));
    // Older peer (unknown): holding while it advertises authority.
    assert_eq!(may_identify(&pair("b", "b", None)), Ok(()));
    let mut old = pair("b", "b", None);
    old.nodes[0].notify_authority = true;
    assert_eq!(may_identify(&old), Err("a".into()));
    // Silent past the peer-silence threshold / fenced stale: no longer blocks.
    let mut silent = pair("b", "b", Some(true));
    silent.nodes[0].fresh = false;
    assert_eq!(may_identify(&silent), Ok(()));
    let mut stale = pair("b", "b", Some(true));
    stale.nodes[0].stale = true;
    assert_eq!(may_identify(&stale), Ok(()));
    // Removed from the membership: ignored.
    let mut removed = pair("b", "b", Some(true));
    removed.members = vec!["b".into()];
    assert_eq!(may_identify(&removed), Ok(()));
    // Cluster off: always.
    assert_eq!(may_identify(&single_node()), Ok(()));
}

/// Drive a handoff tick by tick; each node sees the other's heartbeat from
/// the previous tick. Returns `(old_closed_at, new_identified_at)`.
async fn run_handoff(
    old: &mut Worker,
    new: &mut Worker,
    old_digest: impl Fn(Option<bool>) -> ClusterDigest,
    new_digest: impl Fn(Option<bool>) -> ClusterDigest,
    start: u64,
    gap_check: &mut dyn FnMut(&mut Worker, u64),
) -> (u64, u64) {
    let (mut closed, mut identified) = (None, None);
    for t in start..start + 45 {
        let old_hb = Some(old.gateway.is_some());
        let new_hb = Some(new.gateway.is_some());
        old.apply_digest(&old_digest(new_hb), false, t);
        old.manage_gateway(t);
        new.apply_digest(&new_digest(old_hb), false, t);
        new.manage_gateway(t);
        assert!(
            !(old.gateway.is_some() && new.gateway.is_some()),
            "two sessions at t+{}",
            t - start
        );
        if closed.is_none() && old.gateway.is_none() {
            closed = Some(t);
        }
        if identified.is_none() && new.gateway.is_some() {
            identified = Some(t);
        }
        if new.gateway.is_none() {
            gap_check(new, t);
        }
    }
    (
        closed.expect("old closed"),
        identified.expect("new identified"),
    )
}

/// (a) + (b): the old holder closes after the hysteresis decision, before
/// the new one identifies; in the gap bind is no_writer and gateway_owner
/// names the real holder or null.
#[tokio::test]
async fn handoff_closes_the_old_session_before_the_new_one_identifies() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let now = now_secs();
    let (_ta, _sa, mut a) = worker_on(&mock, "a", OPENID, true).await;
    let (_tb, _sb, mut b) = worker_on(&mock, "b", OPENID, true).await;
    a.apply_digest(&pair("a", "a", Some(false)), false, now);
    a.manage_gateway(now);
    b.apply_digest(&pair("b", "a", Some(true)), false, now);
    b.manage_gateway(now);
    assert!(a.gateway.is_some() && b.gateway.is_none());
    assert_eq!(
        b.status(now, &pair("b", "a", Some(true)))["gateway_owner"]["node_id"],
        "a"
    );
    let mut gap_binds = 0;
    let (closed, identified) = run_handoff(
        &mut a,
        &mut b,
        |held| pair("a", "b", held),
        |held| pair("b", "b", held),
        now + 1,
        &mut |b, t| {
            let refusal = b.start_bind(BindMode::Code, true, t).unwrap_err();
            assert_eq!(refusal.data, json!({"code": "no_writer"}));
            // Undo nothing: the refusal opened no window.
            assert!(!b.binder.waiting(t));
            gap_binds += 1;
        },
    )
    .await;
    assert!(
        closed >= now + 1 + GATEWAY_HYSTERESIS_SECS,
        "hysteresis delays the decision"
    );
    assert!(
        closed < identified,
        "close {closed} before identify {identified}"
    );
    assert!(gap_binds > 0);
    // The new holder: gateway_owner local, bind works.
    let t = identified + 1;
    assert_eq!(
        b.status(t, &pair("b", "b", Some(false)))["gateway_owner"]["is_local"],
        true
    );
    assert!(b.start_bind(BindMode::Code, true, t).is_ok());
}

/// Gap seen from the new writer with nobody holding: gateway_owner null.
#[tokio::test]
async fn gateway_owner_is_null_when_nobody_holds_a_session() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let now = now_secs();
    let (_tb, _sb, mut b) = worker_on(&mock, "b", OPENID, false).await;
    b.apply_digest(&pair("b", "b", Some(false)), false, now);
    b.manage_gateway(now);
    assert!(b.gateway.is_none());
    assert!(b.status(now, &pair("b", "b", Some(false)))["gateway_owner"].is_null());
    // The old holder still reports a session: it is the real holder, and
    // bind on the writer is no_writer.
    b.apply_digest(&pair("b", "b", Some(true)), false, now + 1);
    assert_eq!(b.gateway_holder.as_ref().unwrap().node_id, "a");
    assert_eq!(
        b.start_bind(BindMode::Code, true, now + 1)
            .unwrap_err()
            .data,
        json!({"code": "no_writer"})
    );
}

/// (c) An unreachable old holder (its last heartbeat said held): the new
/// one identifies only once that heartbeat is past the silence threshold
/// or the node is fenced (stale).
#[tokio::test]
async fn unreachable_old_holder_blocks_identify_until_stale_or_fenced() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let now = now_secs();
    for fenced in [false, true] {
        let (_tb, _sb, mut b) = worker_on(&mock, "b", OPENID, true).await;
        let mut digest = pair("b", "b", Some(true));
        for t in now..now + 5 {
            b.apply_digest(&digest, false, t);
            b.manage_gateway(t);
            assert!(b.gateway.is_none(), "still within the silence threshold");
            assert_eq!(
                b.gateway_state.2.as_deref(),
                Some("等待节点 a 关闭 QQ 网关")
            );
        }
        if fenced {
            digest.nodes[0].stale = true;
            digest.nodes[0].healthy = false;
        } else {
            digest.nodes[0].fresh = false;
        }
        b.apply_digest(&digest, false, now + 5);
        b.manage_gateway(now + 5);
        assert!(b.gateway.is_some(), "identifies after fenced={fenced}");
    }
}

/// (d) A flap shorter than the hysteresis: no handoff, the old session is
/// kept (not respawned), and the peer never identifies.
#[tokio::test]
async fn a_flap_during_hysteresis_keeps_the_session_and_hands_off_nothing() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let now = now_secs();
    let (_ta, _sa, mut a) = worker_on(&mock, "a", OPENID, true).await;
    let (_tb, _sb, mut b) = worker_on(&mock, "b", OPENID, true).await;
    a.apply_digest(&pair("a", "a", Some(false)), false, now);
    a.manage_gateway(now);
    let spawned_at = a.gateway_state.1;
    for t in now + 1..now + 60 {
        // Owner flips to b for 10 s, then back.
        let owner = if t < now + 11 { "b" } else { "a" };
        let a_held = Some(a.gateway.is_some());
        let b_held = Some(b.gateway.is_some());
        a.apply_digest(&pair("a", owner, b_held), false, t);
        a.manage_gateway(t);
        b.apply_digest(&pair("b", owner, a_held), false, t);
        b.manage_gateway(t);
        assert!(a.gateway.is_some(), "kept at t+{}", t - now);
        assert!(b.gateway.is_none(), "b never identifies");
    }
    assert_eq!(
        a.gateway_state.1, spawned_at,
        "same session, not reconnected"
    );
    assert!(a.lost_primary_at.is_none());
}

/// (e) Delegate handoff: the owner becomes fully configured; the delegate
/// closes (after the hysteresis) before the owner identifies.
#[tokio::test]
async fn delegate_handoff_back_to_the_owner_closes_before_identify() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let now = now_secs();
    let (_ta, store_a, mut a) = worker_on(&mock, "a", "", true).await;
    let (_tb, _sb, mut b) = worker_on(&mock, "b", OPENID, true).await;
    let digest = |local: &str, owner_capable: bool, peer_held: Option<bool>| {
        let mut digest = cluster(
            local,
            "a",
            vec![slot("a", true, "healthy"), slot("b", true, "healthy")],
        );
        digest.nodes[0].notify_capable = owner_capable;
        digest.nodes[1].notify_authority = !owner_capable;
        for node in &mut digest.nodes {
            if node.id != local {
                node.gateway_held = peer_held;
            }
        }
        digest
    };
    b.apply_digest(&digest("b", false, Some(false)), false, now);
    b.manage_gateway(now);
    a.apply_digest(&digest("a", false, Some(true)), false, now);
    a.manage_gateway(now);
    assert_eq!(b.authority, Authority::Delegate);
    assert!(b.gateway.is_some() && a.gateway.is_none());
    set_openid(&store_a, OPENID);
    a.reload_settings();
    let mut gap = 0;
    let (closed, identified) = run_handoff(
        &mut b,
        &mut a,
        |held| digest("b", true, held),
        |held| digest("a", true, held),
        now + 1,
        &mut |a, t| {
            assert_eq!(
                a.start_bind(BindMode::Code, true, t).unwrap_err().data,
                json!({"code": "no_writer"})
            );
            gap += 1;
        },
    )
    .await;
    assert!(closed >= now + 1 + GATEWAY_HYSTERESIS_SECS);
    assert!(closed < identified);
    assert!(gap > 0);
    assert_eq!(a.authority, Authority::Primary);
}
