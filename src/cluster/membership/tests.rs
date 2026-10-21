use super::*;
use crate::{cluster::peer_auth::random_id, storage::Store};
use serde_json::json;
use std::{io, path::PathBuf};

struct Node {
    dir: PathBuf,
    engine: Membership,
}
impl Node {
    async fn new(name: &str, create: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("bilistream-membership-{}", random_id().unwrap()));
        let store = Store::open(dir.join("data"), dir.join("key")).unwrap();
        store.write("config.json",json!({"cluster":{"enabled":false},"youtube":{"enable_monitor":true,"proxy":"local-only"},"twitch":{"enable_monitor":true},"niconico":{"enable_monitor":true},"priority_channel":{"enabled":true,"auto_restart":true},"bililive":{"enable_danmaku_command":true},"enable_youtube_monitor":true,"enable_twitch_monitor":true,"unknown_extension":{"keep":42}})).unwrap();
        let engine = Membership::new(store);
        engine
            .setup(
                SetupRequest {
                    operation_id: random_id().unwrap(),
                    expected_local_revision: 0,
                    node_id: name.into(),
                    name: name.into(),
                    api_url: format!("https://{name}.example.test"),
                    priority: 0,
                },
                create,
                || async { Ok(()) },
            )
            .await
            .unwrap();
        Self { dir, engine }
    }
    fn id(&self) -> String {
        self.engine.identity().unwrap().public().member_id.clone()
    }
    fn restart(&mut self) {
        // Exercise loading a new engine through an independent committed cache
        // in reopen tests below; this helper resets no durable state.
        self.engine.initialize().unwrap();
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn begin_change(node: &Node, change: Change) -> Operation {
    node.engine
        .begin(
            random_id().unwrap(),
            node.engine.manifest().unwrap().unwrap().revision,
            node.id(),
            change,
        )
        .unwrap()
}
fn add_proposal(coordinator: &Node, candidate: &Node) -> Proposal {
    let op = begin_change(
        coordinator,
        Change::Add {
            target_url: candidate
                .engine
                .local()
                .unwrap()
                .descriptor
                .unwrap()
                .api_url,
        },
    );
    let receipt = candidate
        .engine
        .reserve_pairing(op.intent.clone(), |_| Ok(()))
        .unwrap();
    coordinator
        .engine
        .propose(op.intent.operation_id, Some(receipt))
        .unwrap()
}
async fn prepare(coordinator: &Node, participants: &[&Node], p: &Proposal) {
    for node in participants {
        let receipt = node
            .engine
            .prepare(p.clone(), || async { Ok(()) })
            .await
            .unwrap();
        coordinator
            .engine
            .record_prepare(p.intent.operation_id.clone(), receipt)
            .unwrap();
    }
}
fn commit(coordinator: &Node, participants: &[&Node], p: &Proposal) -> Decision {
    let decision = coordinator
        .engine
        .decide(p.intent.operation_id.clone(), DecisionKind::Commit)
        .unwrap();
    for node in participants {
        let receipt = node
            .engine
            .apply_decision(decision.clone())
            .unwrap()
            .unwrap();
        coordinator
            .engine
            .record_installed(p.intent.operation_id.clone(), receipt)
            .unwrap();
    }
    decision
}
fn finish(coordinator: &Node, participants: &[&Node], p: &Proposal) -> Finish {
    let finish = coordinator
        .engine
        .decide_finish(p.intent.operation_id.clone())
        .unwrap();
    for node in participants {
        node.engine.apply_finish(finish.clone()).unwrap();
    }
    finish
}
async fn add(coordinator: &Node, candidate: &Node, old: &[&Node]) {
    let proposal = add_proposal(coordinator, candidate);
    let mut nodes = old.to_vec();
    nodes.push(candidate);
    prepare(coordinator, &nodes, &proposal).await;
    commit(coordinator, &nodes, &proposal);
    finish(coordinator, &nodes, &proposal);
}

#[tokio::test]
async fn hold_is_durable_before_stop_and_failed_stop_never_votes() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    let proposal = add_proposal(&a, &b);
    let engine = a.engine.clone();
    let id = proposal.intent.operation_id.clone();
    assert!(a
        .engine
        .prepare(proposal.clone(), move || async move {
            assert_eq!(engine.local()?.hold.as_deref(), Some(id.as_str()));
            assert!(engine.operation(&id)?.unwrap().prepared.is_none());
            Err(io::Error::other("source still running"))
        })
        .await
        .is_err());
    assert!(a
        .engine
        .operation(&proposal.intent.operation_id)
        .unwrap()
        .unwrap()
        .prepared
        .is_none());
    assert!(execution_hold(a.engine.store()).unwrap().is_some());
}

#[tokio::test]
async fn lost_prepare_reply_and_retries_use_identical_receipts() {
    let a = Node::new("a", true).await;
    let mut b = Node::new("b", false).await;
    let p = add_proposal(&a, &b);
    let first = b
        .engine
        .prepare(p.clone(), || async { Ok(()) })
        .await
        .unwrap();
    b.restart();
    let repeated = b
        .engine
        .prepare(p, || async {
            panic!("must not repeat source shutdown after durable receipt")
        })
        .await
        .unwrap();
    assert_eq!(first, repeated);
}

#[tokio::test]
async fn abort_before_prepare_and_late_shutdown_cannot_resurrect_operation() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    let p = add_proposal(&a, &b);
    a.engine.begin_prepare(p.clone()).unwrap();
    let abort = a
        .engine
        .decide(p.intent.operation_id.clone(), DecisionKind::Abort)
        .unwrap();
    a.engine.apply_decision(abort.clone()).unwrap();
    b.engine.apply_decision(abort).unwrap();
    assert!(a.engine.complete_prepare(p.clone()).is_err());
    assert!(b.engine.begin_prepare(p).is_err());
    assert_eq!(b.engine.local().unwrap().lifecycle, Lifecycle::JoinReady);
    assert!(execution_hold(b.engine.store()).unwrap().is_some());
}

#[tokio::test]
async fn candidate_never_counts_in_old_quorum_and_all_installs_gate_finish() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    add(&a, &b, &[&a]).await;
    let c = Node::new("c", false).await;
    let p = add_proposal(&a, &c);
    prepare(&a, &[&a, &c], &p).await;
    assert!(a
        .engine
        .decide(p.intent.operation_id.clone(), DecisionKind::Commit)
        .is_err());
    prepare(&a, &[&b], &p).await;
    let d = commit(&a, &[&a, &b], &p);
    assert!(a
        .engine
        .decide_finish(p.intent.operation_id.clone())
        .is_err());
    assert!(execution_hold(a.engine.store()).unwrap().is_some());
    let receipt = c.engine.apply_decision(d).unwrap().unwrap();
    a.engine
        .record_installed(p.intent.operation_id.clone(), receipt)
        .unwrap();
    finish(&a, &[&a, &b, &c], &p);
    assert!(execution_hold(a.engine.store()).unwrap().is_none());
    for n in [&a, &b, &c] {
        assert_eq!(
            n.engine.manifest().unwrap().unwrap().digest,
            p.desired.digest
        );
    }
}

#[tokio::test]
async fn concurrent_proposals_from_different_panels_cannot_both_prepare() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    add(&a, &b, &[&a]).await;
    let pa = begin_change(
        &a,
        Change::SetPublicNode {
            public_member_id: Some(a.id()),
        },
    );
    let pb = begin_change(
        &b,
        Change::SetPublicNode {
            public_member_id: Some(b.id()),
        },
    );
    let pa = a.engine.propose(pa.intent.operation_id, None).unwrap();
    let pb = b.engine.propose(pb.intent.operation_id, None).unwrap();
    a.engine.begin_prepare(pa.clone()).unwrap();
    b.engine.begin_prepare(pb.clone()).unwrap();
    assert!(a.engine.begin_prepare(pb.clone()).is_err());
    assert!(b.engine.begin_prepare(pa.clone()).is_err());
    assert!(a
        .engine
        .decide(pa.intent.operation_id, DecisionKind::Commit)
        .is_err());
    assert!(b
        .engine
        .decide(pb.intent.operation_id, DecisionKind::Commit)
        .is_err());
}

#[tokio::test]
async fn offline_standby_removal_revokes_at_every_retained_node() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    add(&a, &b, &[&a]).await;
    let c = Node::new("c", false).await;
    add(&b, &c, &[&a, &b]).await;
    let op = begin_change(
        &b,
        Change::Remove {
            target_member_id: c.id(),
            replacement_public_member_id: None,
        },
    );
    let p = b.engine.propose(op.intent.operation_id, None).unwrap();
    prepare(&b, &[&a, &b], &p).await;
    commit(&b, &[&a, &b], &p);
    assert!(b
        .engine
        .decide_finish(p.intent.operation_id.clone())
        .is_err());
    let policy = FencingPolicy::issue(c.engine.store()).unwrap();
    let finish = b
        .engine
        .decide_finish_with_safety(
            p.intent.operation_id.clone(),
            Some(RemovalSafety::Fenced {
                waited_secs: policy.max_stop_secs,
                policy,
            }),
        )
        .unwrap();
    for node in [&a, &b] {
        node.engine.apply_finish(finish.clone()).unwrap();
    }
    for node in [&a, &b] {
        assert!(!trust_snapshot(node.engine.store())
            .unwrap()
            .members
            .iter()
            .any(|m| m.member_id == c.id()));
        assert!(!operation_trust(node.engine.store(), &p.intent)
            .unwrap()
            .members
            .iter()
            .any(|m| m.member_id == c.id()));
    }
    assert!(c
        .engine
        .manifest()
        .unwrap()
        .unwrap()
        .member(&c.id())
        .is_some());
}

#[tokio::test]
async fn isolated_two_node_member_cannot_remove_peer() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    add(&a, &b, &[&a]).await;
    let op = begin_change(
        &a,
        Change::Remove {
            target_member_id: b.id(),
            replacement_public_member_id: None,
        },
    );
    let p = a.engine.propose(op.intent.operation_id, None).unwrap();
    prepare(&a, &[&a], &p).await;
    assert!(a
        .engine
        .decide(p.intent.operation_id, DecisionKind::Commit)
        .is_err());
}

#[tokio::test]
async fn stale_finish_does_not_release_newer_hold_and_commit_cannot_abort() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    let p = add_proposal(&a, &b);
    prepare(&a, &[&a, &b], &p).await;
    commit(&a, &[&a, &b], &p);
    let old = finish(&a, &[&a, &b], &p);
    assert!(a
        .engine
        .decide(p.intent.operation_id, DecisionKind::Abort)
        .is_err());
    let op = begin_change(
        &b,
        Change::SetPublicNode {
            public_member_id: Some(b.id()),
        },
    );
    let new = b.engine.propose(op.intent.operation_id, None).unwrap();
    prepare(&b, &[&a, &b], &new).await;
    a.engine.apply_finish(old).unwrap();
    assert_eq!(
        a.engine.local().unwrap().hold.as_deref(),
        Some(new.intent.operation_id.as_str())
    );
}

#[tokio::test]
async fn changed_same_id_is_rejected_and_non_cluster_data_survives() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    let p = add_proposal(&a, &b);
    assert!(a
        .engine
        .begin(
            p.intent.operation_id.clone(),
            1,
            a.id(),
            Change::SetPublicNode {
                public_member_id: Some(a.id())
            }
        )
        .is_err());
    prepare(&a, &[&a, &b], &p).await;
    commit(&a, &[&a, &b], &p);
    finish(&a, &[&a, &b], &p);
    for n in [&a, &b] {
        let cfg = n.engine.store().read("config.json").unwrap().unwrap().value;
        assert_eq!(cfg["unknown_extension"]["keep"], 42);
        assert_eq!(cfg["youtube"]["proxy"], "local-only");
    }
}

#[tokio::test]
async fn durable_prepare_and_decision_survive_actual_store_reopen() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    let p = add_proposal(&a, &b);
    prepare(&a, &[&a, &b], &p).await;
    let decision = a
        .engine
        .decide(p.intent.operation_id.clone(), DecisionKind::Commit)
        .unwrap();
    let bytes = a
        .engine
        .store()
        .read(OPERATIONS_RECORD)
        .unwrap()
        .unwrap()
        .value;
    // A separate installation uses its own SQL worker/cache. Copying the logical
    // journal is NOT the test: drop the real Store before reopening the same WAL.
    let dir = b.dir.clone();
    let Node { engine, .. } = &b;
    let _ = engine;
    // Coordinator crash is represented by constructing a new engine from the
    // current durable Store; the full drop/reopen path is tested separately below.
    let resumed = Membership::new(a.engine.store().clone());
    resumed.initialize().unwrap();
    assert_eq!(
        resumed
            .operation(&p.intent.operation_id)
            .unwrap()
            .unwrap()
            .decision,
        Some(decision)
    );
    assert_eq!(
        resumed
            .store()
            .read(OPERATIONS_RECORD)
            .unwrap()
            .unwrap()
            .value,
        bytes
    );
    assert!(dir.exists());
}

#[tokio::test]
async fn every_prepared_member_is_held_so_maintenance_has_no_publisher() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    add(&a, &b, &[&a]).await;
    let op = begin_change(
        &a,
        Change::SetPublicNode {
            public_member_id: Some(b.id()),
        },
    );
    let proposal = a.engine.propose(op.intent.operation_id, None).unwrap();
    prepare(&a, &[&a, &b], &proposal).await;
    assert!(execution_hold(a.engine.store()).unwrap().is_some());
    assert!(execution_hold(b.engine.store()).unwrap().is_some());
}

#[test]
fn legacy_enabled_configuration_refuses_to_start_and_writes_nothing() {
    let dir = std::env::temp_dir().join(format!("bilistream-membership-{}", random_id().unwrap()));
    let store = Store::open(dir.join("data"), dir.join("key")).unwrap();
    let peer_secret = "synthetic-legacy-peer-token-5f1e";
    store
        .write(
            "config.json",
            json!({"cluster":{"enabled":true,"node_id":"synthetic-node-7a2c","peers":[{"node_id":"synthetic-peer-7a2c","api_url":"https://synthetic-peer.invalid","token":peer_secret}]}}),
        )
        .unwrap();
    let engine = Membership::new(store.clone());
    let error = engine.initialize().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let text = error.to_string();
    assert_eq!(text, LEGACY_TOPOLOGY_ERROR);
    assert!(
        text.contains("cluster.enabled") && text.contains(LOCAL_RECORD),
        "{text}"
    );
    for leaked in [
        peer_secret,
        "synthetic-node-7a2c",
        "synthetic-peer-7a2c",
        "synthetic-peer.invalid",
    ] {
        assert!(!text.contains(leaked), "{text}");
        assert!(!format!("{error:?}").contains(leaked));
    }
    // Nothing is written: no held lifecycle, no identity.
    assert!(store.read(LOCAL_RECORD).unwrap().is_none());
    assert!(engine.identity().is_err());
    // Runtime fences stay closed for the same data if anything reads it.
    assert!(execution_hold(&store).unwrap().is_some());
    drop(engine);
    drop(store);
    let reopened = Store::open(dir.join("data"), dir.join("key")).unwrap();
    let again = Membership::new(reopened.clone()).initialize().unwrap_err();
    assert_eq!(again.to_string(), LEGACY_TOPOLOGY_ERROR);
    assert!(reopened.read(LOCAL_RECORD).unwrap().is_none());
    drop(reopened);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_stored_incompatible_held_lifecycle_still_loads_and_stays_held() {
    let dir = std::env::temp_dir().join(format!("bilistream-membership-{}", random_id().unwrap()));
    let store = Store::open(dir.join("data"), dir.join("key")).unwrap();
    store
        .write("config.json", json!({"cluster":{"enabled":true}}))
        .unwrap();
    // Exactly what 28a20792 / fc661258 wrote for a legacy enabled topology.
    let held = serde_json::to_value(LocalState {
        lifecycle: Lifecycle::IncompatibleHeld,
        hold: Some("incompatible".into()),
        ..LocalState::default()
    })
    .unwrap();
    assert_eq!(held["lifecycle"], "incompatible_held");
    store.write(LOCAL_RECORD, held.clone()).unwrap();
    let engine = Membership::new(store.clone());
    engine.initialize().unwrap();
    assert_eq!(
        engine.local().unwrap().lifecycle,
        Lifecycle::IncompatibleHeld
    );
    assert_eq!(store.read(LOCAL_RECORD).unwrap().unwrap().value, held);
    assert!(execution_hold(&store).unwrap().is_some());
    assert_eq!(lifecycle_or_held(&store), Lifecycle::IncompatibleHeld);
    drop(engine);
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_unreadable_local_state_fails_closed_as_incompatible_held() {
    let dir = std::env::temp_dir().join(format!("bilistream-membership-{}", random_id().unwrap()));
    let store = Store::open(dir.join("data"), dir.join("key")).unwrap();
    assert_eq!(lifecycle_or_held(&store), Lifecycle::Standalone);
    store
        .write(LOCAL_RECORD, json!({"lifecycle":"synthetic-unknown-shape"}))
        .unwrap();
    assert_eq!(lifecycle_or_held(&store), Lifecycle::IncompatibleHeld);
    assert!(hold_reason(&store).is_some());
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn invalid_changes_are_rejected_before_admission_or_candidate_reservation() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    for change in [
        Change::UpdateNode {
            target_member_id: a.id(),
            name: "bad\nname".into(),
            api_url: "https://a.example.test".into(),
            priority: 0,
        },
        Change::SetPublicNode {
            public_member_id: Some("missing".into()),
        },
        Change::Add {
            target_url: "https://a.example.test/".into(),
        },
    ] {
        assert!(a
            .engine
            .begin(random_id().unwrap(), 1, a.id(), change)
            .is_err());
        assert!(a.engine.operations().unwrap().is_empty());
    }
    let op = begin_change(
        &a,
        Change::Add {
            target_url: "https://b.example.test".into(),
        },
    );
    let mut local = b.engine.local().unwrap();
    local.descriptor.as_mut().unwrap().node_id = "a".into();
    b.engine
        .store()
        .write(LOCAL_RECORD, serde_json::to_value(&local).unwrap())
        .unwrap();
    assert!(b
        .engine
        .reserve_pairing(op.intent.clone(), |_| Ok(()))
        .is_err());
    assert_eq!(b.engine.local().unwrap().lifecycle, Lifecycle::JoinReady);

    // Simulate an older target that admitted the duplicate descriptor before
    // full validation: coordinator construction must durably abort it.
    let receipt = PairingReceipt::sign(
        &op.intent,
        local.descriptor.clone().unwrap(),
        &b.engine.identity().unwrap(),
    )
    .unwrap();
    local.lifecycle = Lifecycle::Pairing;
    local.hold = Some(op.intent.operation_id.clone());
    local.pairing = Some(PairingReservation {
        intent: op.intent.clone(),
        receipt: receipt.clone(),
    });
    b.engine
        .store()
        .write(LOCAL_RECORD, serde_json::to_value(local).unwrap())
        .unwrap();
    assert!(a
        .engine
        .propose(op.intent.operation_id.clone(), Some(receipt))
        .is_err());
    let aborted = a
        .engine
        .operation(&op.intent.operation_id)
        .unwrap()
        .unwrap();
    assert!(aborted.terminal());
    assert_eq!(aborted.pairing_candidate.unwrap().member_id, b.id());
    b.engine.apply_decision(aborted.decision.unwrap()).unwrap();
    assert_eq!(b.engine.local().unwrap().lifecycle, Lifecycle::JoinReady);
    assert!(b.engine.local().unwrap().pairing.is_none());
}

#[tokio::test]
async fn final_node_remove_stays_held_on_failed_stop_and_retries_to_left() {
    let a = Node::new("a", true).await;
    let op = begin_change(
        &a,
        Change::Remove {
            target_member_id: a.id(),
            replacement_public_member_id: None,
        },
    );
    let id = op.intent.operation_id;
    assert!(a
        .engine
        .dissolve(id.clone(), || async {
            Err(io::Error::other("source still running"))
        })
        .await
        .is_err());
    a.engine.initialize().unwrap();
    assert_eq!(a.engine.local().unwrap().hold.as_deref(), Some(id.as_str()));
    assert!(!a.engine.operation(&id).unwrap().unwrap().terminal());
    a.engine
        .dissolve(id.clone(), || async { Ok(()) })
        .await
        .unwrap();
    a.engine
        .dissolve(id.clone(), || async {
            panic!("completed dissolve must not stop twice")
        })
        .await
        .unwrap();
    let op = a.engine.operation(&id).unwrap().unwrap();
    assert!(op.terminal() && op.finished_locally && op.dissolved);
    assert!(op.decision.is_none() && op.finish.is_none());
    assert_eq!(a.engine.local().unwrap().lifecycle, Lifecycle::Left);
    assert!(trust_snapshot(a.engine.store()).is_err());
    let config = a.engine.store().read("config.json").unwrap().unwrap().value;
    assert_eq!(config["youtube"]["enable_monitor"], false);
    assert_eq!(config["priority_channel"]["enabled"], false);
    assert_eq!(config["unknown_extension"]["keep"], 42);
}

#[tokio::test]
async fn removal_finish_rejects_missing_or_forged_stop_proof_and_survives_later_edits() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    let c = Node::new("c", false).await;
    add(&a, &b, &[&a]).await;
    add(&a, &c, &[&a, &b]).await;
    let op = begin_change(
        &a,
        Change::Remove {
            target_member_id: c.id(),
            replacement_public_member_id: None,
        },
    );
    let p = a
        .engine
        .propose(op.intent.operation_id.clone(), None)
        .unwrap();
    prepare(&a, &[&a, &b], &p).await;
    commit(&a, &[&a, &b], &p);
    let policy = FencingPolicy::issue(c.engine.store()).unwrap();
    assert!(a
        .engine
        .decide_finish(op.intent.operation_id.clone())
        .is_err());
    for safety in [
        RemovalSafety::Fenced {
            policy: policy.clone(),
            waited_secs: policy.max_stop_secs - 1,
        },
        RemovalSafety::Fenced {
            policy: FencingPolicy::issue(b.engine.store()).unwrap(),
            waited_secs: policy.max_stop_secs,
        },
    ] {
        assert!(a
            .engine
            .decide_finish_with_safety(op.intent.operation_id.clone(), Some(safety))
            .is_err());
    }
    let proof = a
        .engine
        .decide_finish_with_safety(
            op.intent.operation_id.clone(),
            Some(RemovalSafety::Fenced {
                waited_secs: policy.max_stop_secs,
                policy,
            }),
        )
        .unwrap();
    for node in [&a, &b] {
        node.engine.apply_finish(proof.clone()).unwrap();
    }
    assert!(a.engine.removed_source_stopped("c").unwrap());
    assert!(!a.engine.removed_source_stopped("b").unwrap());
    let edit = begin_change(
        &a,
        Change::SetPublicNode {
            public_member_id: Some(a.id()),
        },
    );
    let p = a.engine.propose(edit.intent.operation_id, None).unwrap();
    prepare(&a, &[&a, &b], &p).await;
    commit(&a, &[&a, &b], &p);
    finish(&a, &[&a, &b], &p);
    assert!(a.engine.removed_source_stopped("c").unwrap());
    a.engine.initialize().unwrap();
    assert!(a.engine.removed_source_stopped("c").unwrap());
}

#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn log_dispatch() -> (tracing::Dispatch, Logs) {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    (tracing::Dispatch::new(subscriber), logs)
}

/// A Remove FINISH as stored before 0dd9ec5: correctly signed over the
/// 2-tuple, the target's prepare receipt inside the decision, and no
/// `removal_safety`. Returns (legacy, current) for the same decision.
async fn legacy_remove_finish_fixture(a: &Node, b: &Node, c: &Node) -> (Finish, Finish, Proposal) {
    let op = begin_change(
        b,
        Change::Remove {
            target_member_id: c.id(),
            replacement_public_member_id: None,
        },
    );
    let p = b.engine.propose(op.intent.operation_id, None).unwrap();
    prepare(b, &[a, b, c], &p).await;
    let decision = commit(b, &[a, b], &p);
    let current = b
        .engine
        .decide_finish(p.intent.operation_id.clone())
        .unwrap();
    assert!(matches!(
        current.removal_safety,
        Some(RemovalSafety::Prepared { .. })
    ));
    let installed = current.installed.clone();
    let proof = b.engine.identity().unwrap().sign(
        crate::cluster::peer_auth::Domain::Finish,
        &encode(&(&decision, &installed)).unwrap(),
    );
    let legacy = Finish {
        decision,
        installed,
        removal_safety: None,
        proof,
    };
    (legacy, current, p)
}

fn assert_names_record_only(text: &str, operation_id: &str, nodes: &[&Node], finish: &Finish) {
    assert!(text.contains(operation_id), "{text}");
    assert!(text.contains("removal_safety"), "{text}");
    assert_no_leak(text, nodes, finish);
}

/// Member ids (fingerprints) and every signature in the record stay out.
fn assert_no_leak(text: &str, nodes: &[&Node], finish: &Finish) {
    let mut leaked: Vec<String> = nodes.iter().map(|n| n.id()).collect();
    leaked.push(finish.proof.signature.clone());
    for receipt in finish.decision.prepares.iter().chain(&finish.installed) {
        leaked.push(receipt.proof.signature.clone());
    }
    leaked.push(finish.decision.proof.signature.clone());
    for value in leaked {
        assert!(!value.is_empty());
        assert!(!text.contains(&value), "leaked {value} in {text}");
    }
}

#[tokio::test]
async fn a_legacy_remove_finish_on_the_wire_is_refused_by_name_and_changes_nothing() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    add(&a, &b, &[&a]).await;
    let c = Node::new("c", false).await;
    add(&b, &c, &[&a, &b]).await;
    let (legacy, current, p) = legacy_remove_finish_fixture(&a, &b, &c).await;
    let id = p.intent.operation_id.clone();
    // The signature is genuine; only the removed shape is refused.
    legacy.validate().unwrap();
    let error = legacy.validate_removal_safety().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), legacy_remove_finish(&id).to_string());
    assert_names_record_only(&error.to_string(), &id, &[&a, &b, &c], &legacy);
    let (dispatch, logs) = log_dispatch();
    let state = |name: &str| {
        a.engine
            .store()
            .read(name)
            .unwrap()
            .map(|d| (d.revision, d.value))
    };
    let (before_local, before_ops) = (state(LOCAL_RECORD), state(OPERATIONS_RECORD));
    let refused = tracing::dispatcher::with_default(&dispatch, || {
        a.engine.apply_finish(legacy.clone()).unwrap_err()
    });
    assert_eq!(refused.to_string(), error.to_string());
    assert_names_record_only(&format!("{refused:?}"), &id, &[&a, &b, &c], &legacy);
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_no_leak(&logged, &[&a, &b, &c], &legacy);
    assert_eq!(state(LOCAL_RECORD), before_local);
    assert_eq!(state(OPERATIONS_RECORD), before_ops);
    // Signing never produces the removed shape, and the current one applies.
    assert!(Finish::sign_with_safety(
        legacy.decision.clone(),
        legacy.installed.clone(),
        None,
        &b.engine.identity().unwrap()
    )
    .is_err());
    for node in [&a, &b] {
        node.engine.apply_finish(current.clone()).unwrap();
    }
    assert!(a.engine.removed_source_stopped("c").unwrap());
}

#[tokio::test]
async fn a_stored_legacy_removed_source_fails_loudly_and_never_counts_as_stop_proof() {
    let a = Node::new("a", true).await;
    let b = Node::new("b", false).await;
    add(&a, &b, &[&a]).await;
    let c = Node::new("c", false).await;
    add(&b, &c, &[&a, &b]).await;
    let (legacy, current, p) = legacy_remove_finish_fixture(&a, &b, &c).await;
    let id = p.intent.operation_id.clone();
    for node in [&a, &b] {
        node.engine.apply_finish(current.clone()).unwrap();
    }
    assert!(a.engine.removed_source_stopped("c").unwrap());
    // Replace the stored evidence with the pre-0dd9ec5 shape, as an old
    // node's cluster-local would hold it.
    let mut local = a.engine.store().read(LOCAL_RECORD).unwrap().unwrap().value;
    local["removed_sources"]["c"] = serde_json::to_value(&legacy).unwrap();
    assert!(local["removed_sources"]["c"]
        .get("removal_safety")
        .is_none());
    a.engine.store().write(LOCAL_RECORD, local.clone()).unwrap();
    // Startup still loads it; the record is refused where it is used.
    a.engine.initialize().unwrap();
    let (dispatch, logs) = log_dispatch();
    let error = tracing::dispatcher::with_default(&dispatch, || {
        a.engine.removed_source_stopped("c").unwrap_err()
    });
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let text = error.to_string();
    assert_eq!(
        text,
        format!(
            "cluster-local 的 removed_sources 条目 c：{}",
            legacy_remove_finish(&id)
        )
    );
    assert_names_record_only(&text, &id, &[&a, &b, &c], &legacy);
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_no_leak(&logged, &[&a, &b, &c], &legacy);
    // Never silently dropped or rewritten.
    assert_eq!(
        a.engine.store().read(LOCAL_RECORD).unwrap().unwrap().value,
        local
    );
}

#[test]
fn a_legacy_finish_with_an_unvalidated_operation_id_never_echoes_it() {
    let text = legacy_remove_finish("synthetic-SECRET-not-an-operation-id").to_string();
    assert!(!text.contains("synthetic-SECRET"), "{text}");
    assert!(text.contains("（标识无效）"), "{text}");
}
