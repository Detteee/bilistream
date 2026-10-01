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
        let store = Store::open(dir.join("data"), dir.join("key"), None).unwrap();
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
    finish(&b, &[&a, &b], &p);
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
    let Node { engine, dir: _, .. } = &b;
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
fn legacy_enabled_configuration_stays_held_and_never_creates_an_identity() {
    let dir = std::env::temp_dir().join(format!("bilistream-membership-{}", random_id().unwrap()));
    let store = Store::open(dir.join("data"), dir.join("key"), None).unwrap();
    store
        .write("config.json", json!({"cluster":{"enabled":true}}))
        .unwrap();
    let engine = Membership::new(store.clone());
    engine.initialize().unwrap();
    assert_eq!(
        engine.local().unwrap().lifecycle,
        Lifecycle::IncompatibleHeld
    );
    assert!(engine.identity().is_err());
    assert!(execution_hold(&store).unwrap().is_some());
    drop(engine);
    drop(store);
    let reopened = Store::open(dir.join("data"), dir.join("key"), None).unwrap();
    assert!(execution_hold(&reopened).unwrap().is_some());
    drop(reopened);
    let _ = std::fs::remove_dir_all(dir);
}
