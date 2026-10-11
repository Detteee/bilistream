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
    cfg.qq.admin_openid = openid.into();
    cfg.qq.api_base = mock.base.clone();
    cfg.qq.token_url = mock.token_url();
    cfg.qq.keep_online = false;
    store::save_config(store, &cfg, None).unwrap();
    store::save_secret(store, Some("synthetic-secret"), None).unwrap();
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
    assert!(store::load_config(&store)
        .unwrap()
        .0
        .qq
        .admin_openid
        .is_empty());

    // The mocked gateway delivers the code.
    worker
        .handle_gateway_event(GatewayEvent::C2c {
            openid: OPENID.into(),
            msg_id: "m-1".into(),
            content: format!("绑定 {code}"),
            at: now + 1,
        })
        .await;
    assert_eq!(
        store::load_config(&store).unwrap().0.qq.admin_openid,
        OPENID
    );
    let messages = mock.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, OPENID);
    assert_eq!(
        messages[0].1,
        json!({"msg_type": 0, "content": "绑定成功", "msg_id": "m-1", "msg_seq": 1})
    );
    assert_eq!(worker.binder.view(now + 2, "solo")["state"], "bound");
    assert!(worker.capable());

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
    assert_eq!(
        store::load_config(&store).unwrap().0.qq.admin_openid,
        OPENID
    );
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
    assert!(store::load_config(&store)
        .unwrap()
        .0
        .qq
        .admin_openid
        .is_empty());
    assert!(mock.messages().is_empty());
    worker.start_bind(BindMode::Any, false, now).unwrap();
    worker
        .handle_gateway_event(GatewayEvent::FriendAdd {
            openid: OPENID.into(),
            event_id: Some("FRIEND_ADD:1".into()),
            at: now,
        })
        .await;
    assert_eq!(
        store::load_config(&store).unwrap().0.qq.admin_openid,
        OPENID
    );
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
    assert_eq!(refusal.owner_node.as_deref(), Some("a"));
    assert!(refusal.reason.contains('a'));
}

#[tokio::test]
async fn bind_needs_saved_credentials() {
    let (_temp, store) = store();
    let mut worker = Worker::new(Arc::clone(&store), "solo".into());
    let refusal = worker
        .start_bind(BindMode::Any, false, now_secs())
        .unwrap_err();
    assert!(refusal.reason.contains("AppSecret"));
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
            content: "状态".into(),
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
            content: "状态".into(),
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
    let (mut cfg, rev) = store::load_config(&store).unwrap();
    cfg.qq.admin_openid = "NEWADMIN0000000000000000000000000".into();
    store::save_config(&store, &cfg, rev).unwrap();
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
    assert!(refusal.reason.contains("确认"));
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
