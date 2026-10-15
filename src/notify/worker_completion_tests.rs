//! Original acceptance gaps: actual observer/worker/sink paths with a fake
//! clock, loopback QQ and isolated child processes; never production nodes.

use super::*;
use crate::events::{Event as Fact, EventKind as FactKind, NodeState};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

fn capture(cap: usize) -> (Arc<crate::events::Bus>, mpsc::Receiver<Arc<Fact>>) {
    let bus = Arc::new(crate::events::Bus::new());
    let rx = bus.register("history", cap, |_| true);
    bus.seal();
    (bus, rx)
}

fn drain(rx: &mut mpsc::Receiver<Arc<Fact>>) -> Vec<Arc<Fact>> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn attempts(rx: &mut mpsc::Receiver<Arc<Fact>>) -> Vec<Arc<Fact>> {
    drain(rx)
        .into_iter()
        .filter(|event| matches!(event.kind, FactKind::NotifySent { .. }))
        .collect()
}

#[tokio::test]
async fn recorder_records_without_qq_and_closes_at_the_last_observation_on_handoff() {
    crate::install_crypto_provider();
    let (_temp, store) = store();
    let mut worker = Worker::new(store, "a".into());
    assert!(!worker.cfg.enabled && !worker.capable());
    let now = now_secs();
    let (bus, mut rx) = capture(128);
    crate::events::with_test_bus(bus, async {
        let mut d = cluster("a", "a", vec![slot("a", true, "healthy"), slot("b", true, "healthy"), slot("c", false, "waiting_for_heartbeat")]);
        worker.apply_digest(&d, false, now);
        let first = drain(&mut rx);
        assert_eq!(first.iter().filter(|event| matches!(event.kind, FactKind::NodeState { initial: true, .. })).count(), 3);
        assert!(first.iter().any(|event| matches!(&event.kind, FactKind::NodeState { node_id, state: NodeState::Unknown, .. } if node_id == "c")));
        assert_eq!(worker.recorder, Authority::Primary);
        assert_eq!(worker.authority, Authority::None);
        worker.apply_digest(&d, false, now + 5);
        assert!(drain(&mut rx).iter().all(|event| matches!(event.kind, FactKind::RecorderHeartbeat)));
        d.nodes[1] = slot("b", false, "heartbeat_timeout");
        worker.apply_digest(&d, false, now + 10);
        assert!(drain(&mut rx).iter().any(|event| matches!(&event.kind, FactKind::NodeState { node_id, state: NodeState::Down, initial: false, .. } if node_id == "b")));
        d.handoff_pending = true;
        d.fence_block = Some("handoff".into());
        worker.apply_digest(&d, false, now + 25);
        let lost = drain(&mut rx);
        assert_eq!(lost.len(), 1);
        assert!(matches!(lost[0].kind, FactKind::RecorderStopped));
        assert_eq!(lost[0].at, now + 10, "no invented coverage during handoff");
        worker.apply_digest(&d, false, now + 30);
        assert!(drain(&mut rx).is_empty());
        d.handoff_pending = false;
        d.fence_block = None;
        worker.apply_digest(&d, false, now + 50);
        assert_eq!(drain(&mut rx).iter().filter(|event| matches!(event.kind, FactKind::NodeState { initial: true, .. })).count(), 3);
        worker.apply_digest(&d, true, now + 51);
        assert_eq!(drain(&mut rx)[0].at, now + 50, "membership pause closes rather than recording failure");
    }).await;
}

#[tokio::test]
async fn recorder_minority_election_ignores_credentials_and_does_not_call_unseen_peers_down() {
    crate::install_crypto_provider();
    let now = now_secs();
    let mut chosen = Vec::new();
    for local in ["a", "b"] {
        let (_temp, store) = store();
        let mut worker = Worker::new(store, local.into());
        worker.last_quorum_seen = now;
        worker.cfg.strict_single_sender = true; // only QQ, not history
        let mut d = cluster(
            local,
            "b",
            vec![
                slot("a", true, "healthy"),
                slot("b", true, "healthy"),
                slot("c", false, "heartbeat_timeout"),
            ],
        );
        d.quorum = false;
        d.fence_block = Some("quorum".into());
        for node in &mut d.nodes {
            node.notify_capable = false;
        }
        let (bus, mut rx) = capture(64);
        crate::events::with_test_bus(bus, async {
            worker.apply_digest(&d, false, now + 119);
            assert!(drain(&mut rx).is_empty());
            worker.apply_digest(&d, false, now + 120);
            chosen.push(worker.recorder);
            let facts = drain(&mut rx);
            if local == "a" {
                assert!(facts.iter().any(|fact| matches!(&fact.kind, FactKind::NodeState { node_id, state: NodeState::Unknown, minority: true, .. } if node_id == "c")));
                assert!(facts.iter().any(|fact| matches!(fact.kind, FactKind::QuorumChanged { has_quorum: false })));
            } else { assert!(facts.is_empty()); }
        }).await;
    }
    assert_eq!(chosen, [Authority::Fallback, Authority::None]);
}

#[tokio::test]
async fn disabled_recorder_retains_the_failed_owner_across_an_ownerless_handoff() {
    crate::install_crypto_provider();
    let (_temp, store) = store();
    let mut worker = Worker::new(store, "b".into());
    assert!(!worker.cfg.enabled && !worker.capable());
    let now = now_secs();
    let mut digest = cluster(
        "b",
        "a",
        vec![slot("a", true, "healthy"), slot("b", true, "healthy")],
    );
    digest.fence_block = Some("standby".into());
    let (bus, mut rx) = capture(32);
    crate::events::with_test_bus(bus, async {
        worker.apply_digest(&digest, false, now);
        assert!(drain(&mut rx).is_empty());
        digest.owner = None;
        digest.handoff_pending = true;
        digest.nodes[0] = slot("a", false, "heartbeat_timeout");
        worker.apply_digest(&digest, false, now + 5);
        assert!(drain(&mut rx).is_empty());
        digest.owner = Some("b".into());
        digest.handoff_pending = false;
        digest.fence_block = None;
        worker.apply_digest(&digest, false, now + 10);
        let facts = drain(&mut rx);
        assert!(facts.iter().any(|event| matches!(
            &event.kind,
            FactKind::OwnerChanged { from: Some(from), to: Some(to), failover: true }
                if from == "a" && to == "b"
        )));
        assert_eq!(worker.recorder, Authority::Primary);
        assert_eq!(worker.authority, Authority::None);
    })
    .await;
}

#[test]
fn stalled_live_intake_is_bounded_observable_and_returns_under_a_millisecond() {
    let bus = Arc::new(crate::events::Bus::new());
    let mut notify = bus.register(
        "notify",
        crate::notify::COMMAND_CAPACITY,
        crate::notify::bus::wants,
    );
    let mut other = bus.register("other", 4, |kind| {
        matches!(kind, FactKind::RecorderHeartbeat)
    });
    bus.seal();
    const N: usize = 20_000;
    let mut nanos = Vec::with_capacity(N);
    crate::events::with_sync_test_bus(Arc::clone(&bus), || {
        for _ in 0..N {
            let start = std::time::Instant::now();
            crate::notify::live_ended();
            nanos.push(start.elapsed().as_nanos());
        }
        crate::events::emit(FactKind::RecorderHeartbeat);
        assert_eq!(
            crate::notify::status_view()["dropped"]["events"],
            (N - crate::notify::COMMAND_CAPACITY) as u64,
            "the status read exposes drops even before the worker wakes"
        );
    });
    assert_eq!(drain(&mut notify).len(), crate::notify::COMMAND_CAPACITY);
    assert_eq!(
        bus.dropped()
            .into_iter()
            .find(|(name, _)| *name == "notify")
            .unwrap()
            .1,
        (N - crate::notify::COMMAND_CAPACITY) as u64
    );
    assert_eq!(
        drain(&mut other).len(),
        1,
        "a stalled notifier does not block another consumer"
    );
    nanos.sort_unstable();
    let p99 = nanos[N * 99 / 100];
    let mean = nanos.iter().sum::<u128>() / N as u128;
    eprintln!(
        "live bus saturated intake: mean={mean}ns p99={p99}ns max={}ns",
        nanos[N - 1]
    );
    assert!(p99 < 1_000_000 && mean < 1_000_000);
}

#[tokio::test]
async fn every_actual_auth_or_content_retry_has_one_safe_history_attempt() {
    crate::install_crypto_provider();
    for (status, code) in [(401, 11241), (400, 304003)] {
        let mock = MockQq::start().await;
        mock.script([(status, json!({"code": code, "trace_id": "first-request"}))]);
        let (_temp, store) = store();
        configure(&store, &mock, OPENID);
        let mut worker = ready_worker(&store, "solo").await;
        let (bus, mut rx) = capture(32);
        crate::events::with_test_bus(bus, async {
            assert!(worker.test_send(123_000).await.ok);
        })
        .await;
        assert_eq!(mock.messages().len(), 2);
        for (_, body) in mock.messages() {
            let content = body["content"].as_str().unwrap();
            assert!(content.ends_with("发送节点：solo"), "{content}");
            assert_eq!(content.matches("发送节点：").count(), 1);
        }
        let facts = attempts(&mut rx);
        assert_eq!(
            facts.len(),
            2,
            "inner retry must not disappear into final success"
        );
        assert!(
            matches!(&facts[0].kind, FactKind::NotifySent { key, dedup, phase: "test", mode: Some("active"), ok: false, code: Some(got), test: true, trace_id: Some(trace), .. } if key == "notify.test" && dedup == "notify.test:solo" && *got == code && trace == "first-request")
        );
        assert!(matches!(
            facts[1].kind,
            FactKind::NotifySent {
                ok: true,
                test: true,
                ..
            }
        ));
        let debug = format!("{facts:?}");
        for secret in [
            OPENID,
            "synthetic-secret",
            "synthetic-token",
            "这是一条 bilistream",
        ] {
            assert!(!debug.contains(secret));
        }
    }
}

#[tokio::test]
async fn cancellation_after_c2c_dispatch_is_one_unknown_attempt_but_local_quota_is_zero() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    mock.state.lock().unwrap().message_delay = Duration::from_millis(300);
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    let (bus, mut rx) = capture(32);
    crate::events::with_test_bus(bus, async {
        let result = tokio::time::timeout(Duration::from_millis(100), worker.test_send(now)).await;
        assert!(result.is_err());
        assert_eq!(
            mock.messages().len(),
            1,
            "request reached the loopback server"
        );
        let facts = attempts(&mut rx);
        assert_eq!(facts.len(), 1);
        assert!(matches!(
            facts[0].kind,
            FactKind::NotifySent {
                ok: false,
                code: None,
                ..
            }
        ));
        for _ in 0..crate::notify::ledger::ACTIVE_PER_MINUTE {
            worker.ledger.record_active(now);
        }
        worker.passive = None;
        let report = worker.test_send(now).await;
        assert!(!report.ok);
        assert!(
            attempts(&mut rx).is_empty(),
            "local quota refusal is not a C2C request"
        );
        assert_eq!(mock.messages().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn broadcast_bus_keeps_generation_dedup_and_tracks_each_ladder_attempt() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    mock.script([
        (400, json!({"code":40034105})),
        (400, json!({"code":304103})),
    ]);
    let (_temp, store) = store();
    configure(&store, &mock, OPENID);
    let mut worker = ready_worker(&store, "solo").await;
    let now = now_secs();
    let bus = Arc::new(crate::events::Bus::new());
    let mut rx = bus.register("history", 128, |_| true);
    let mut intake = bus.register("notify", 64, crate::notify::bus::wants);
    bus.seal();
    crate::events::with_test_bus(bus, async {
        worker.apply_digest(&single_node(), false, now);
        worker.passive = Some(PassiveTarget {
            msg_id: "admin-window".into(),
            received_at: now,
            replies_used: 0,
        });
        worker.ledger.record_admin_message(now);
        for _ in 0..2 {
            crate::notify::live_started(crate::notify::live_events::LiveStartInfo {
                stream_key: "youtube:UCfixture".into(),
                platform: "YouTube".into(),
                channel: "Configured channel".into(),
                title: "Fixture".into(),
            });
            let event = intake.recv().await.unwrap();
            worker.on_live_signal(crate::notify::bus::map(&event).unwrap(), now);
            worker.deliver_live(now).await;
        }
        assert_eq!(
            mock.messages().len(),
            3,
            "one active, one passive, one recall; duplicate start stays silent"
        );
        let facts = attempts(&mut rx);
        let modes: Vec<_> = facts
            .iter()
            .map(|event| match &event.kind {
                FactKind::NotifySent {
                    phase, mode, ok, ..
                } => (*phase, *mode, *ok),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            modes,
            [
                ("live_start", Some("active"), false),
                ("live_start", Some("passive"), false),
                ("live_start", Some("wakeup"), true)
            ]
        );
        let generation = worker.ledger.live.broadcast.generation;
        crate::notify::live_ended();
        let event = intake.recv().await.unwrap();
        worker.on_live_signal(crate::notify::bus::map(&event).unwrap(), now + 3 * 3600);
        assert_eq!(worker.ledger.live.broadcast.generation, generation);
    })
    .await;
}

#[tokio::test]
async fn a_held_relay_notice_names_the_new_sender_after_handoff() {
    use crate::notify::live_events::{LiveSignal, LiveStartInfo};
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    mock.script([(400, json!({"code":40034105}))]);
    let (_ta, sa) = store();
    let (_tb, sb) = store();
    configure(&sa, &mock, OPENID);
    configure(&sb, &mock, OPENID);
    let mut a = ready_worker(&sa, "a").await;
    let mut b = ready_worker(&sb, "b").await;
    let now = now_secs();
    a.apply_digest(&pair("a", "a", Some(false)), false, now);
    assert_eq!(a.authority, Authority::Primary);
    a.node_names.insert("a".into(), "旧发送节点".into());
    a.ledger.record_admin_message(now - 4 * 86_400);
    a.on_live_signal(
        LiveSignal::Started(LiveStartInfo {
            stream_key: "youtube:UCfixture".into(),
            platform: "YouTube".into(),
            channel: "示例频道".into(),
            title: "https://example.com <qqbot-at-user/>\n".repeat(20),
        }),
        now,
    );
    a.deliver_live(now).await;
    assert_eq!(mock.messages().len(), 1);
    assert_eq!(a.ledger.live.pending(), ["live_start"]);
    assert_eq!(
        a.ledger.wakeups_left(now),
        Some(2),
        "held notice leaves the reserve intact"
    );

    let later = now + 600;
    a.apply_digest(&pair("a", "b", Some(false)), false, later);
    assert_eq!(a.authority, Authority::None);
    a.deliver_live(later).await;
    assert_eq!(mock.messages().len(), 1, "the previous sender cannot retry");
    let mut digest = pair("b", "b", Some(false));
    digest
        .nodes
        .iter_mut()
        .find(|node| node.id == "b")
        .unwrap()
        .name = "新发送节点".into();
    digest.peer_ledgers.push(a.ledger.clone());
    b.apply_digest(&digest, false, later);
    assert_eq!(b.authority, Authority::Primary);
    b.ledger.record_admin_message(later);
    b.passive = Some(PassiveTarget {
        msg_id: "new-holder-admin-window".into(),
        received_at: later,
        replies_used: 0,
    });
    mock.script([(400, json!({"code":40034105}))]);
    b.deliver_live(later).await;
    b.deliver_live(later + 1).await;
    let messages = mock.messages();
    assert_eq!(
        messages.len(),
        3,
        "one refused old send, then active and passive on B"
    );
    assert!(messages[0].1["content"]
        .as_str()
        .unwrap()
        .ends_with("发送节点：旧发送节点"));
    for (_, body) in &messages[1..] {
        let content = body["content"].as_str().unwrap();
        assert!(content.starts_with("🟢 开始转播 示例频道"));
        assert!(content.contains(&format!("补发：开始于 {}", format::utc8_time(now))));
        assert!(content.ends_with("发送节点：新发送节点"));
        assert!(!content.contains("旧发送节点") && !content.contains("代发"));
        assert!(!content.contains("https://") && !content.contains("<qqbot-at-user"));
        assert!(content.chars().count() <= format::MAX_CHARS);
    }
    assert_eq!(messages[2].1["msg_id"], "new-holder-admin-window");
    assert!(b.ledger.live.pending().is_empty());
    assert_eq!(
        b.ledger.wakeups_left(later),
        Some(4),
        "held notice never spends a wakeup"
    );
}

// A child process emits pulses until killed. The parent feeds that observed
// liveness into the real cluster normalization/election/fencing pipeline.
// No shell, external runtime, QQ host or production service is involved.
#[test]
fn node_process() {
    if std::env::var("BILISTREAM_NOTIFY_NODE_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    use std::io::Write;
    for _ in 0..15_000 {
        if writeln!(std::io::stdout(), "pulse").is_err() {
            return;
        }
        if std::io::stdout().flush().is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct NodeProcess {
    child: std::process::Child,
    ticks: Arc<AtomicUsize>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl NodeProcess {
    async fn start() -> Self {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "notify::worker::tests::completion::node_process",
                "--nocapture",
            ])
            .env("BILISTREAM_NOTIFY_NODE_FIXTURE", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let reader = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if line == "pulse" {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        let mut process = Self {
            child,
            ticks,
            reader: Some(reader),
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while process.ticks.load(Ordering::Relaxed) == 0 {
                assert!(
                    process.child.try_wait().unwrap().is_none(),
                    "child fixture exited early"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        process
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

struct TestNode {
    _temp: Temp,
    worker: Worker,
    config: crate::config::Config,
    state: crate::cluster::ClusterState,
    process: NodeProcess,
    connected: bool,
    self_check_fail: bool,
    bus: Arc<crate::events::Bus>,
    history: mpsc::Receiver<Arc<Fact>>,
}

struct TestCluster {
    nodes: BTreeMap<String, TestNode>,
    now: u64,
    live: Vec<crate::notify::live::LiveObservation>,
}

impl TestCluster {
    async fn new(ids: &[&str], mock: &MockQq) -> Self {
        let now = now_secs();
        let mut nodes = BTreeMap::new();
        for (i, id) in ids.iter().enumerate() {
            let (temp, store) = store();
            configure(&store, mock, OPENID);
            let mut config = crate::cluster::tests::test_config(id, 100 - i as i32);
            config.cluster.failover_timeout_secs = 5;
            config.cluster.peers = ids
                .iter()
                .enumerate()
                .filter(|(_, peer)| *peer != id)
                .map(|(n, peer)| crate::config::ClusterPeer {
                    node_id: (*peer).into(),
                    name: (*peer).into(),
                    api_url: "http://127.0.0.1:9".into(),
                    priority: 100 - n as i32,
                })
                .collect();
            let mut worker = ready_worker(&store, id).await;
            worker.cluster_cfg = Some(config.clone());
            let (bus, history) = capture(4096);
            nodes.insert(
                (*id).into(),
                TestNode {
                    _temp: temp,
                    worker,
                    config,
                    state: crate::cluster::ClusterState {
                        active_owner: Some(ids[0].to_string()),
                        ..Default::default()
                    },
                    process: NodeProcess::start().await,
                    connected: true,
                    self_check_fail: false,
                    bus,
                    history,
                },
            );
        }
        Self {
            nodes,
            now,
            live: Vec::new(),
        }
    }

    /// One synthetic heartbeat interval, interpreted by the actual cluster
    /// election and actual Worker. `handoff_complete` acknowledges a stopped
    /// former owner only after a test observed the new owner's hold.
    async fn pulse(&mut self, offset: u64, handoff_complete: bool) {
        let now = self.now + offset;
        let members: HashSet<_> = self.nodes.keys().cloned().collect();
        let heartbeats: Vec<_> = self
            .nodes
            .iter_mut()
            .filter_map(|(id, node)| {
                (node.process.alive() && node.connected).then(|| {
                    let mut snapshot = crate::cluster::empty_node(
                        id,
                        id,
                        "",
                        node.config.cluster.priority,
                        false,
                        now,
                    );
                    snapshot.last_seen = Some(now);
                    snapshot.health = crate::cluster::ClusterHealth::healthy();
                    snapshot.notify_capable = node.worker.capable();
                    snapshot.notify_authority = node.worker.authority != Authority::None;
                    snapshot.notify_gateway_held = Some(false);
                    snapshot.notify = Some(node.worker.ledger.clone());
                    if node.self_check_fail {
                        snapshot.self_check = Some(crate::cluster::SelfCheckStatus {
                            state: crate::cluster::SelfCheckState::Failing,
                            consecutive_failures: 3,
                            checked_at: Some(now),
                            last_success_at: None,
                            latency_ms: None,
                            failure: None,
                        });
                    }
                    (id.clone(), snapshot, node.state.active_owner.clone())
                })
            })
            .collect();
        for (id, node) in &mut self.nodes {
            if !node.process.alive() || !node.connected {
                continue;
            }
            for (peer, snapshot, owner) in &heartbeats {
                let mut snapshot = snapshot.clone();
                snapshot.is_local = peer == id;
                node.state.nodes.insert(peer.clone(), snapshot);
                if peer != id {
                    node.state.peer_heartbeat_acks.insert(peer.clone(), now);
                    node.state.peer_owner_views.insert(
                        peer.clone(),
                        crate::cluster::PeerOwnerView {
                            owner: owner.clone(),
                            received_at: now,
                            members: members.clone(),
                            confirmed_by_heartbeat: true,
                        },
                    );
                }
            }
            if handoff_complete && node.state.pending_handoff_source.is_some() {
                node.state.pending_handoff_source = None;
                node.state.local_execution_held = false;
            }
            crate::cluster::build_status_from_state(
                &mut node.state,
                &node.config,
                String::new(),
                now,
            );
        }
        for node in self.nodes.values_mut() {
            if !node.process.alive() || !node.connected {
                continue;
            }
            *crate::cluster::cluster_state_write() = node.state.clone();
            let digest = node.worker.digest(now);
            crate::events::with_test_bus(Arc::clone(&node.bus), async {
                node.worker.apply_digest(&digest, false, now);
                node.worker.channel_live(now, &self.live);
                node.worker.deliver(now, &digest).await;
            })
            .await;
        }
    }

    fn facts(&mut self) -> Vec<(String, Arc<Fact>)> {
        self.nodes
            .iter_mut()
            .flat_map(|(id, node)| {
                drain(&mut node.history)
                    .into_iter()
                    .map(|event| (id.clone(), event))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[tokio::test]
async fn two_node_survivor_delivers_one_labelled_fallback_and_strict_mode_stays_silent() {
    let _guard = crate::cluster::tests::ClusterStateGuard::new();
    crate::install_crypto_provider();
    for strict in [false, true] {
        let mock = MockQq::start().await;
        let mut nodes = TestCluster::new(&["a", "b"], &mock).await;
        nodes
            .nodes
            .get_mut("a")
            .unwrap()
            .worker
            .cfg
            .strict_single_sender = strict;
        nodes.pulse(0, true).await;
        nodes.nodes.get_mut("b").unwrap().process.stop();
        for t in [6, 95, 96, 106, 119] {
            nodes.pulse(t, true).await;
        }
        assert!(mock.messages().is_empty(), "no fallback before grace");
        nodes.pulse(120, true).await;
        nodes.pulse(140, true).await;
        nodes.pulse(240, true).await;
        let messages = mock.messages();
        assert_eq!(messages.len(), usize::from(!strict));
        let survivor = &nodes.nodes["a"].worker;
        assert_eq!(
            survivor.recorder,
            Authority::Fallback,
            "recording is independent of the QQ switch"
        );
        if !strict {
            let text = messages[0].1["content"].as_str().unwrap();
            assert!(text.contains("无多数派") && text.ends_with("发送节点：a"));
            assert!(!text.contains("b 已失去连接"));
        }
        let facts = nodes.facts();
        assert_eq!(
            facts
                .iter()
                .filter(|(_, event)| matches!(event.kind, FactKind::NotifySent { .. }))
                .count(),
            usize::from(!strict)
        );
    }
}

#[tokio::test]
async fn owner_process_failover_reuses_the_delivered_fault_and_linked_watch_ledger() {
    let _guard = crate::cluster::tests::ClusterStateGuard::new();
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let mut nodes = TestCluster::new(&["a", "b", "c"], &mock).await;
    nodes.pulse(0, true).await;
    nodes.nodes.get_mut("c").unwrap().self_check_fail = true;
    for t in [10, 70, 80] {
        nodes.pulse(t, true).await;
    }
    assert_eq!(
        mock.messages().len(),
        1,
        "A sent the existing self-check incident"
    );
    for node in nodes.nodes.values_mut() {
        node.worker.store.write("channels.json", json!({"channels":[{"name":"Configured channel","aliases":[],"platforms":{"youtube":"UCfixture","twitch":"fixture-login"}}]})).unwrap();
        let (mut cfg, revision) = store::load_config(&node.worker.store).unwrap();
        cfg.watch.enabled = true;
        cfg.watch.channels = vec![crate::notify::config::WatchChannel::Configured {
            name: "Configured channel".into(),
        }];
        store::save_config(&node.worker.store, &cfg, revision).unwrap();
        node.worker.reload_settings();
    }
    nodes.live = vec![
        crate::notify::live::LiveObservation {
            platform: "youtube".into(),
            channel_id: "UCfixture".into(),
            title: Some("same episode".into()),
        },
        crate::notify::live::LiveObservation {
            platform: "twitch".into(),
            channel_id: "fixture-login".into(),
            title: Some("same episode".into()),
        },
    ];
    for t in [90, 100, 101] {
        nodes.pulse(t, true).await;
    }
    assert_eq!(
        mock.messages().len(),
        2,
        "both platforms yield one channel notice"
    );
    nodes.nodes.get_mut("a").unwrap().process.stop();
    nodes.pulse(107, false).await;
    nodes.pulse(108, false).await;
    assert!(nodes.nodes["b"].state.pending_handoff_source.is_some());
    assert_eq!(nodes.nodes["b"].worker.authority, Authority::None);
    assert_eq!(
        mock.messages().len(),
        2,
        "handoff hold permits no new sender"
    );
    for t in [120, 130, 180, 190, 220] {
        nodes.pulse(t, true).await;
    }
    let messages = mock.messages();
    assert_eq!(
        messages.len(),
        4,
        "one old fault, one watched episode, one failover and one confirmed old-owner loss"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|(_, body)| body["content"]
                .as_str()
                .unwrap()
                .contains("🟢 Configured channel 开播了"))
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|(_, body)| body["content"]
                .as_str()
                .unwrap()
                .contains("转播节点已从 a 切换到 b"))
            .count(),
        1
    );
    let facts = nodes.facts();
    let sent: Vec<_> = facts
        .iter()
        .filter_map(|(node, event)| match &event.kind {
            FactKind::NotifySent {
                key,
                dedup,
                ok: true,
                ..
            } => Some((node.as_str(), key.as_str(), dedup.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 4);
    assert_eq!(
        sent.iter()
            .filter(|(_, key, _)| *key == "cluster.self_check_failing")
            .count(),
        1,
        "new sender must not repeat the imported incident"
    );
    assert_eq!(
        sent.iter()
            .filter(|(node, key, _)| *node == "b" && *key == "cluster.failover")
            .count(),
        1
    );
    assert_eq!(nodes.nodes["b"].worker.ledger.undelivered_pending(), 0);
    assert_eq!(
        facts
            .iter()
            .filter(|(node, event)| node == "b"
                && matches!(
                    &event.kind,
                    FactKind::OwnerChanged { from: Some(from), to: Some(to), failover: true }
                        if from == "a" && to == "b"
                ))
            .count(),
        1,
        "the recorder retains the failed owner across its execution hold"
    );
}

#[tokio::test]
async fn twenty_second_flaps_for_ten_minutes_produce_one_warning_and_one_recovery_with_suppression()
{
    let _guard = crate::cluster::tests::ClusterStateGuard::new();
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let mut nodes = TestCluster::new(&["a", "b", "c"], &mock).await;
    nodes.pulse(0, true).await;
    for t in 1..=600 {
        nodes.nodes.get_mut("c").unwrap().connected = ((t - 1) / 20) % 2 == 1;
        nodes.pulse(t, true).await;
    }
    nodes.nodes.get_mut("c").unwrap().connected = true;
    for t in 601..=650 {
        nodes.pulse(t, true).await;
    }
    assert_eq!(
        mock.messages().len(),
        1,
        "flap recovery doubles the stable hold, then coalesces"
    );
    nodes.pulse(651, true).await;
    nodes.pulse(700, true).await;
    let messages = mock.messages();
    assert_eq!(
        messages.len(),
        2,
        "full ten-minute schedule stays inside one cooldown"
    );
    let text: Vec<_> = messages
        .iter()
        .map(|(_, body)| body["content"].as_str().unwrap())
        .collect();
    assert!(text[0].contains("状态反复"));
    assert!(text[1].starts_with("✅"));
    let facts = nodes.facts();
    let counts: Vec<_> = facts
        .iter()
        .filter_map(|(_, event)| match event.kind {
            FactKind::NotifySent { suppressed, .. } => Some(suppressed),
            _ => None,
        })
        .collect();
    assert_eq!(counts.len(), 2);
    assert!(
        counts.iter().sum::<u32>() > 0,
        "repeated appearances are accounted for"
    );
}

#[tokio::test]
async fn refused_delivery_is_counted_and_the_next_success_reports_it_once() {
    let _guard = crate::cluster::tests::ClusterStateGuard::new();
    use tracing::instrument::WithSubscriber;
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    mock.script([
        (400, json!({"code":40034105,"trace_id":"refusal-trace"})),
        (400, json!({"code":40034122,"trace_id":"wakeup-refusal"})),
    ]);
    let mut nodes = TestCluster::new(&["a", "b", "c"], &mock).await;
    nodes.pulse(0, true).await;
    nodes.nodes.get_mut("c").unwrap().process.stop();
    let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
    async {
        for t in [6, 66, 76, 90, 100] {
            nodes.pulse(t, true).await;
        }
    }
    .with_subscriber(dispatch)
    .await;
    assert_eq!(
        mock.messages().len(),
        2,
        "active and the permitted unknown-cycle wakeup both refused; no reroute"
    );
    let a = &nodes.nodes["a"].worker;
    assert_eq!(a.ledger.undelivered_pending(), 1);
    assert_eq!(
        a.status(nodes.now + 100, &a.digest(nodes.now + 100))["local_last_send"]["code"],
        40034122
    );
    let log = logs.text();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("WARN") && line.contains("QQ 通知无法送达"))
            .count(),
        1
    );
    assert!(log.contains("40034105") && log.contains("refusal-trace"));
    assert!(log.contains("40034122") && log.contains("wakeup-refusal"));
    for secret in [OPENID, "synthetic-secret", "synthetic-token"] {
        assert!(!log.contains(secret));
    }
    // A new admin message opens a real passive window; enabled status is
    // answered once, then a new observed incident carries the undelivered count.
    let now = nodes.now;
    let a = nodes.nodes.get_mut("a").unwrap();
    *crate::cluster::cluster_state_write() = a.state.clone();
    crate::events::with_test_bus(Arc::clone(&a.bus), async {
        a.worker
            .handle_gateway_event(GatewayEvent::C2c {
                openid: OPENID.into(),
                msg_id: "fresh-admin".into(),
                content: "status".into(),
                at: now + 101,
            })
            .await;
        a.worker
            .handle_gateway_event(GatewayEvent::C2c {
                openid: "STRANGER".into(),
                msg_id: "stranger".into(),
                content: "status".into(),
                at: now + 102,
            })
            .await;
    })
    .await;
    assert_eq!(mock.messages().len(), 3);
    assert_eq!(mock.messages()[2].1["msg_id"], "fresh-admin");
    nodes.nodes.get_mut("b").unwrap().self_check_fail = true;
    for t in [110, 170, 180, 200] {
        nodes.pulse(t, true).await;
    }
    let messages = mock.messages();
    assert_eq!(messages.len(), 4);
    assert!(messages[3].1["content"]
        .as_str()
        .unwrap()
        .contains("另有 1 条通知未送达"));
    assert_eq!(nodes.nodes["a"].worker.ledger.undelivered_pending(), 0);
    let facts = nodes.facts();
    assert_eq!(
        facts
            .iter()
            .filter(|(_, event)| matches!(event.kind, FactKind::NotifySent { .. }))
            .count(),
        4
    );
    assert_eq!(
        facts
            .iter()
            .filter(|(_, event)| matches!(event.kind, FactKind::NotifySent { ok: false, .. }))
            .count(),
        2
    );
    assert!(facts.iter().any(|(_, event)| matches!(
        event.kind,
        FactKind::NotifySent {
            phase: "command",
            ..
        }
    )));
}

#[tokio::test]
async fn qq_failures_do_not_delay_cluster_status_or_a_running_relay_process() {
    let _guard = crate::cluster::tests::ClusterStateGuard::new();
    use tracing::instrument::WithSubscriber;
    crate::install_crypto_provider();
    for scenario in ["wrong_secret", "unreachable", "stalled"] {
        let mock = MockQq::start().await;
        if scenario == "wrong_secret" {
            mock.state.lock().unwrap().token_reply = Some((200, json!({"code":100016})));
        } else if scenario == "stalled" {
            mock.state.lock().unwrap().message_delay = Duration::from_millis(300);
            mock.script([(503, json!({}))]);
        }
        let mut nodes = TestCluster::new(&["a", "b", "c"], &mock).await;
        if scenario == "unreachable" {
            // Acquire and close a loopback port: no public networking.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
            for node in nodes.nodes.values_mut() {
                node.worker.cfg.qq.api_base = base.clone();
            }
        }
        nodes.pulse(0, true).await;
        nodes.nodes.get_mut("c").unwrap().self_check_fail = true;
        nodes.pulse(10, true).await;
        nodes.pulse(70, true).await;
        let relay = NodeProcess::start().await;
        let ticks = relay.ticks.load(Ordering::Relaxed);
        let cfg = nodes.nodes["a"].config.clone();
        let (dispatch, logs) = crate::logging::test_capture::subscriber(tracing::Level::DEBUG);
        let ((), reads) = async {
            tokio::join!(
                async {
                    for t in 80..=100 {
                        nodes.pulse(t, true).await;
                    }
                },
                async {
                    let mut latencies = Vec::new();
                    for _ in 0..40 {
                        let began = std::time::Instant::now();
                        // The production cluster snapshot path used by the
                        // status API remains responsive while QQ is pending.
                        let status =
                            crate::cluster::compute_cluster_status_view(&cfg, String::new());
                        assert_eq!(status.nodes.len(), 3);
                        let _json = serde_json::to_vec(&status).unwrap();
                        let _notify = crate::notify::status_view();
                        latencies.push(began.elapsed());
                        tokio::time::sleep(Duration::from_millis(3)).await;
                    }
                    latencies
                }
            )
        }
        .with_subscriber(dispatch)
        .await;
        assert!(
            relay.ticks.load(Ordering::Relaxed) > ticks + 10,
            "synthetic relay kept progressing under {scenario}"
        );
        assert_eq!(reads.len(), 40);
        assert!(
            reads
                .iter()
                .all(|elapsed| *elapsed < Duration::from_millis(100)),
            "status calls blocked behind {scenario}"
        );
        let log = logs.text();
        for secret in [OPENID, "synthetic-secret", "synthetic-token"] {
            assert!(!log.contains(secret));
        }
        if scenario == "wrong_secret" {
            assert_eq!(
                mock.state.lock().unwrap().token_calls,
                3,
                "each newly selected sender discovers the invalid secret once"
            );
            assert!(
                mock.messages().is_empty(),
                "token failure is not a message attempt"
            );
            assert_eq!(
                log.lines()
                    .filter(|line| line.contains("WARN") && line.contains("100016"))
                    .count(),
                3
            );
            for node in nodes.nodes.values() {
                assert!(node.worker.bad_credentials);
                assert_eq!(
                    node.worker.last_outcome.as_ref().unwrap().code,
                    Some(100016)
                );
            }
            assert!(nodes
                .facts()
                .iter()
                .all(|(_, event)| !matches!(event.kind, FactKind::NotifySent { .. })));
        } else {
            let facts = nodes.facts();
            assert!(facts
                .iter()
                .any(|(_, event)| matches!(event.kind, FactKind::NotifySent { ok: false, .. })));
        }
    }
}

#[test]
fn history_only_facts_never_create_secondary_qq_alerts() {
    let kinds = [
        FactKind::BiliStartFailed {
            seq: None,
            error: "fixture".into(),
        },
        FactKind::BiliWarning {
            seq: None,
            room: 42,
            msg: "fixture".into(),
        },
        FactKind::BiliCutOff {
            seq: None,
            room: 42,
            msg: "fixture".into(),
        },
        FactKind::RoomLocked { room: 42, until: 1 },
        FactKind::LocalFault {
            reason: "fixture".into(),
            latched: true,
        },
        FactKind::OwnerChanged {
            from: Some("a".into()),
            to: Some("b".into()),
            failover: true,
        },
        FactKind::RecorderStopped,
    ];
    for kind in kinds {
        assert!(!crate::notify::bus::wants(&kind));
        assert!(crate::notify::bus::map(&Fact { at: 1, kind }).is_none());
    }
}

#[tokio::test]
async fn stopped_non_owner_process_sends_exactly_one_failure_and_one_timed_recovery() {
    let _guard = crate::cluster::tests::ClusterStateGuard::new();
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let mut nodes = TestCluster::new(&["a", "b", "c"], &mock).await;
    nodes.pulse(0, true).await;
    nodes.nodes.get_mut("c").unwrap().process.stop();
    nodes.pulse(6, true).await; // actual heartbeat timeout starts the incident
    nodes.pulse(65, true).await;
    assert!(mock.messages().is_empty(), "confirm delay has not elapsed");
    nodes.pulse(66, true).await;
    nodes.pulse(76, true).await;
    assert_eq!(mock.messages().len(), 1);
    assert!(mock.messages()[0].1["content"]
        .as_str()
        .unwrap()
        .starts_with("🔴 c 已失去连接"));
    nodes.nodes.get_mut("c").unwrap().process = NodeProcess::start().await;
    nodes.pulse(126, true).await;
    nodes.pulse(155, true).await;
    assert_eq!(
        mock.messages().len(),
        1,
        "recovery needs a stable healthy hold"
    );
    nodes.pulse(156, true).await;
    nodes.pulse(166, true).await;
    nodes.pulse(200, true).await;
    let messages = mock.messages();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].1["content"].as_str().unwrap().starts_with("✅"));
    assert!(messages[1].1["content"]
        .as_str()
        .unwrap()
        .contains("2 分钟"));
    let facts = nodes.facts();
    let sent: Vec<_> = facts
        .iter()
        .filter(|(_, event)| matches!(event.kind, FactKind::NotifySent { .. }))
        .collect();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|(node, _)| node == "a"));
    let changes: Vec<_> = facts
        .iter()
        .filter_map(|(recorder, event)| match &event.kind {
            FactKind::NodeState {
                node_id,
                state,
                initial: false,
                ..
            } if node_id == "c" => Some((recorder.as_str(), *state, event.at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes,
        [
            ("a", NodeState::Down, nodes.now + 6),
            ("a", NodeState::Up, nodes.now + 126)
        ]
    );
}
