//! Runtime around the transaction engine: one local driver per coordinated
//! operation, participant request handling and process hooks. Every network
//! wait happens outside Store transactions and config locks.

use super::*;
use crate::cluster::peer_auth::{
    PeerClient, PeerResponse, PeerScope, PublicIdentity, RecognitionAnswer, RecognitionProbe,
    RecognitionVerdict, RoutePolicy,
};
use crate::cluster::peer_call::{routes, send_to_member};
use crate::storage::Store;
use futures_util::future::{join_all, BoxFuture};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PEER_TIMEOUT: Duration = Duration::from_secs(15);
const DRIVER_ROUNDS: u32 = 6;

/// Ordinary peer handlers run under a read guard; installing a membership
/// decision takes the write guard, so a revoked sender cannot commit after
/// the revocation it raced with.
pub(crate) static MEMBERSHIP_GATE: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

pub type StopFn = Arc<dyn Fn() -> BoxFuture<'static, io::Result<()>> + Send + Sync>;

#[derive(Clone)]
pub struct Runtime {
    pub membership: Membership,
    pub client: Arc<PeerClient>,
    stop: StopFn,
    process_hooks: bool,
    fence_waits: Arc<Mutex<HashMap<String, tokio::time::Instant>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    pub intent: Intent,
    #[serde(default)]
    pub retry: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatusReply {
    pub status: Option<OperationStatus>,
    pub pairing: Option<PairingReceipt>,
    pub decision: Option<Decision>,
    pub finish: Option<Finish>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRequest {
    pub operation_id: String,
    pub expected_revision: u64,
    pub change: Change,
}

/// The password is consumed by the single reserve call and never persisted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingRequest {
    pub intent: Intent,
    pub password: String,
}

impl std::fmt::Debug for PairingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingRequest(<redacted>)")
    }
}

/// Outcome of the password-bearing reservation as seen by the coordinator.
pub enum Reservation {
    Reserved(PairingReceipt),
    /// The target answered and refused; nothing was reserved.
    Refused(u16, String),
    /// The request may have reached the target; recover through STATUS.
    Uncertain(String),
}

enum Step {
    Done,
    Again,
    Wait(String),
}

enum Reply<T> {
    Ok(T),
    Refused(String),
    Unavailable(String),
}

impl Runtime {
    pub fn new(membership: Membership, client: Arc<PeerClient>, stop: StopFn) -> Self {
        Self {
            membership,
            client,
            stop,
            process_hooks: false,
            fence_waits: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn process() -> io::Result<Self> {
        Ok(Self {
            membership: Membership::new(crate::storage::global()?),
            client: crate::cluster::peer_call::peer_client()?,
            stop: Arc::new(|| Box::pin(stop_local_execution())),
            process_hooks: true,
            fence_waits: process_fence_waits(),
        })
    }

    fn me(&self) -> io::Result<String> {
        Ok(self.membership.identity()?.public().member_id.clone())
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(Membership) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let membership = self.membership.clone();
        tokio::task::spawn_blocking(move || f(membership))
            .await
            .map_err(|_| io::Error::other("成员事务工作线程失败"))?
    }

    async fn call<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        member: &Descriptor,
        intent: &Intent,
        route: RoutePolicy,
        body: &B,
    ) -> Reply<R> {
        let body = match encode(body) {
            Ok(body) => body,
            Err(error) => return Reply::Refused(error.to_string()),
        };
        match send_to_member(
            &self.membership,
            &self.client,
            member,
            &intent.base.cluster_id,
            intent.base.revision,
            PeerScope::Operation(intent.operation_id.clone()),
            route,
            body,
            PEER_TIMEOUT,
        )
        .await
        {
            Ok(response) if response.status == 200 => {
                match serde_json::from_slice(&response.body) {
                    Ok(value) => Reply::Ok(value),
                    Err(_) => Reply::Refused("节点回执格式无效".into()),
                }
            }
            Ok(response) if response.status == 409 => Reply::Refused(
                String::from_utf8_lossy(&response.body)
                    .chars()
                    .take(200)
                    .collect(),
            ),
            Ok(response) => Reply::Unavailable(format!("HTTP {}", response.status)),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Reply::Refused(error.to_string())
            }
            Err(error) => Reply::Unavailable(error.to_string()),
        }
    }

    // ----- coordinator -----

    /// One local driver per operation; a second spawn for a running driver is
    /// a no-op. Exhausted rounds leave the durable phase for the recovery
    /// worker or an operator retry.
    pub fn spawn_driver(&self, id: String) {
        let key = format!("{}|{id}", self.membership.store().data_dir().display());
        if !running()
            .lock()
            .is_ok_and(|mut set| set.insert(key.clone()))
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.drive(&id).await;
            if let Ok(mut set) = running().lock() {
                set.remove(&key);
            }
            if let Err(error) = result {
                tracing::warn!("成员操作 {id} 暂停: {error}");
                let message = error.to_string();
                let _ = this.blocking(move |m| m.note(id, message)).await;
            }
        });
    }

    pub fn driver_running(&self, id: &str) -> bool {
        let key = format!("{}|{id}", self.membership.store().data_dir().display());
        running().lock().is_ok_and(|set| set.contains(&key))
    }

    #[cfg(test)]
    pub(crate) fn advance_fencing_wait_for_test(&self, id: &str, seconds: u64) -> bool {
        let key = format!("{}|{id}", self.membership.store().data_dir().display());
        let mut waits = self.fence_waits.lock().unwrap();
        let Some(start) = waits.get_mut(&key) else {
            return false;
        };
        *start -= Duration::from_secs(seconds);
        true
    }
    #[cfg(test)]
    pub(crate) async fn drive_once_for_test(&self, id: &str) -> io::Result<()> {
        self.step(id).await.map(|_| ())
    }

    pub async fn drive(&self, id: &str) -> io::Result<()> {
        let mut round = 0;
        loop {
            match self.step(id).await? {
                Step::Done => return Ok(()),
                Step::Again => continue,
                Step::Wait(message) => {
                    let (id, note) = (id.to_owned(), message.clone());
                    self.blocking(move |m| m.note(id, note)).await?;
                    round += 1;
                    if round >= DRIVER_ROUNDS {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(round.min(4)))).await;
                }
            }
        }
    }

    async fn step(&self, id: &str) -> io::Result<Step> {
        let op = self
            .membership
            .operation(id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "成员操作不存在"))?;
        let me = self.me()?;
        if op.intent.coordinator != me {
            return Ok(Step::Done);
        }
        if op.intent.is_dissolve() {
            self.membership
                .dissolve(id.to_owned(), || (self.stop)())
                .await?;
            self.membership_changed();
            return Ok(Step::Done);
        }
        if let Some(finish) = &op.finish {
            return self.deliver_finish(&op, finish, &me).await;
        }
        match op.decision.as_ref().map(|d| d.kind) {
            Some(DecisionKind::Abort) => return self.deliver_abort(&op, &me).await,
            Some(DecisionKind::Commit) => return self.install(&op, &me).await,
            None => {}
        }
        let Some(proposal) = op.proposal.clone() else {
            return self.recover_pairing(&op).await;
        };
        self.prepare_all(&op, &proposal, &me).await
    }

    async fn recover_pairing(&self, op: &Operation) -> io::Result<Step> {
        let Change::Add { target_url } = &op.intent.change else {
            let id = op.intent.operation_id.clone();
            self.blocking(move |m| m.propose(id, None)).await?;
            return Ok(Step::Again);
        };
        let endpoint = member_endpoint(target_url)?;
        let identity =
            match tokio::time::timeout(PEER_TIMEOUT, self.client.discover(&endpoint)).await {
                Ok(Ok(identity)) => identity,
                _ => return Ok(Step::Wait("目标服务器暂不可达，无法确认配对状态".into())),
            };
        let candidate = candidate_descriptor(&identity, target_url);
        let (id, pinned) = (op.intent.operation_id.clone(), candidate.clone());
        self.blocking(move |m| m.pin_candidate(id, pinned)).await?;
        let request = StatusRequest {
            intent: op.intent.clone(),
            retry: false,
        };
        match self
            .call::<_, StatusReply>(&candidate, &op.intent, routes::STATUS, &request)
            .await
        {
            Reply::Ok(StatusReply {
                pairing: Some(receipt),
                ..
            }) if receipt.verify(&op.intent).is_ok()
                && receipt.candidate.member_id == identity.member_id =>
            {
                let id = op.intent.operation_id.clone();
                self.blocking(move |m| m.propose(id, Some(receipt))).await?;
                Ok(Step::Again)
            }
            Reply::Unavailable(message) => Ok(Step::Wait(format!("目标服务器暂不可达: {message}"))),
            // The target holds no reservation for this intent: abort. The
            // password was not kept, so the operator starts a new operation.
            _ => {
                let id = op.intent.operation_id.clone();
                self.blocking(move |m| {
                    m.decide(id.clone(), DecisionKind::Abort)?;
                    m.note(id, "目标服务器未保留配对，操作已取消".into())
                })
                .await?;
                Ok(Step::Again)
            }
        }
    }

    async fn prepare_all(&self, op: &Operation, proposal: &Proposal, me: &str) -> io::Result<Step> {
        let pending: Vec<Descriptor> = proposal
            .destinations
            .iter()
            .filter(|m| !op.prepares.contains_key(&m.member_id))
            .cloned()
            .collect();
        let results = join_all(pending.iter().map(|member| async move {
            if member.member_id == me {
                let receipt = self
                    .membership
                    .prepare(proposal.clone(), || (self.stop)())
                    .await;
                (
                    member,
                    receipt.map_or_else(|e| refusal_or_wait(&e), Reply::Ok),
                )
            } else {
                (
                    member,
                    self.call::<_, PreparedReceipt>(member, &op.intent, routes::PREPARE, proposal)
                        .await,
                )
            }
        }))
        .await;
        let mut refused = Vec::new();
        let mut waiting = Vec::new();
        for (member, result) in results {
            match result {
                Reply::Ok(receipt) => {
                    let id = op.intent.operation_id.clone();
                    if let Err(error) = self.blocking(move |m| m.record_prepare(id, receipt)).await
                    {
                        refused.push(format!("{}: {error}", member.node_id));
                    }
                }
                Reply::Refused(message) => refused.push(format!("{}: {message}", member.node_id)),
                Reply::Unavailable(message) => {
                    waiting.push(format!("{}: {message}", member.node_id))
                }
            }
        }
        let op = self
            .membership
            .operation(&op.intent.operation_id)?
            .ok_or_else(denied)?;
        let signers: HashSet<&str> = op.prepares.keys().map(String::as_str).collect();
        let base = &proposal.intent.base.members;
        let old = base
            .iter()
            .filter(|m| signers.contains(m.member_id.as_str()))
            .count();
        let id = op.intent.operation_id.clone();
        if old > base.len() / 2
            && proposal
                .desired
                .members
                .iter()
                .all(|m| signers.contains(m.member_id.as_str()))
        {
            self.blocking(move |m| m.decide(id, DecisionKind::Commit))
                .await?;
            return Ok(Step::Again);
        }
        if !refused.is_empty() {
            let message = format!("节点拒绝准备，操作已取消: {}", refused.join("；"));
            self.blocking(move |m| {
                m.decide(id.clone(), DecisionKind::Abort)?;
                m.note(id, message)
            })
            .await?;
            return Ok(Step::Again);
        }
        Ok(Step::Wait(format!(
            "等待节点停机准备: {}",
            waiting.join("；")
        )))
    }

    async fn install(&self, op: &Operation, me: &str) -> io::Result<Step> {
        let decision = op.decision.clone().ok_or_else(denied)?;
        let proposal = decision.proposal.clone().ok_or_else(denied)?;
        let mut waiting = Vec::new();
        for member in &proposal.destinations {
            let retained = proposal.desired.member(&member.member_id).is_some();
            if !retained && !op.prepares.contains_key(&member.member_id) {
                // A source that returns after COMMIT can still stop durably.
                // Its independent receipt is included in FINISH, without
                // rewriting the already irreversible decision certificate.
                if let Reply::Ok(receipt) = self
                    .call::<_, PreparedReceipt>(member, &op.intent, routes::PREPARE, &proposal)
                    .await
                {
                    let id = op.intent.operation_id.clone();
                    self.blocking(move |m| m.record_prepare(id, receipt))
                        .await?;
                }
            }
            if retained && op.installations.contains_key(&member.member_id) {
                continue;
            }
            let reply = if member.member_id == me {
                match self.apply_decision(decision.clone()).await {
                    Ok(receipt) => Reply::Ok(receipt),
                    Err(error) => refusal_or_wait(&error),
                }
            } else {
                self.call::<_, Option<InstalledReceipt>>(
                    member,
                    &op.intent,
                    routes::DECISION,
                    &decision,
                )
                .await
            };
            let id = op.intent.operation_id.clone();
            match reply {
                Reply::Ok(Some(receipt)) if retained => {
                    self.blocking(move |m| m.record_installed(id, receipt))
                        .await?;
                }
                // Departing members are best-effort here; FINISH re-sends.
                Reply::Ok(_) if !retained => {}
                Reply::Ok(_) => waiting.push(format!("{}: 未返回安装回执", member.node_id)),
                Reply::Refused(message) | Reply::Unavailable(message) if retained => {
                    waiting.push(format!("{}: {message}", member.node_id))
                }
                // Revocation at retained members does not depend on the
                // departing node; its cleanup is reported separately.
                _ => {}
            }
        }
        let op = self
            .membership
            .operation(&op.intent.operation_id)?
            .ok_or_else(denied)?;
        if proposal
            .desired
            .members
            .iter()
            .all(|m| op.installations.contains_key(&m.member_id))
        {
            let safety = match &op.intent.change {
                Change::Remove {
                    target_member_id, ..
                } if !op.prepares.contains_key(target_member_id) => {
                    let member = op.intent.base.member(target_member_id).ok_or_else(denied)?;
                    let Some(policy) = self.membership.fencing_policy(member)? else {
                        return Ok(Step::Wait(format!(
                            "{} 未提供受支持的停机界限证明；请恢复该节点连接并升级后重试",
                            member.node_id
                        )));
                    };
                    let key = format!(
                        "{}|{}",
                        self.membership.store().data_dir().display(),
                        op.intent.operation_id
                    );
                    let elapsed = {
                        let mut waits = self
                            .fence_waits
                            .lock()
                            .map_err(|_| io::Error::other("停机等待状态不可用"))?;
                        waits
                            .entry(key)
                            .or_insert_with(tokio::time::Instant::now)
                            .elapsed()
                    };
                    if elapsed < Duration::from_secs(policy.max_stop_secs) {
                        return Ok(Step::Wait(format!(
                            "等待 {} 的旧成员执行权限停止（还需 {} 秒）",
                            member.node_id,
                            policy.max_stop_secs - elapsed.as_secs()
                        )));
                    }
                    Some(RemovalSafety::Fenced {
                        waited_secs: policy.max_stop_secs,
                        policy,
                    })
                }
                Change::Remove {
                    target_member_id, ..
                } => op
                    .prepares
                    .get(target_member_id)
                    .map(|receipt| RemovalSafety::Prepared {
                        receipt: receipt.clone(),
                    }),
                _ => None,
            };
            let id = op.intent.operation_id.clone();
            self.blocking(move |m| m.decide_finish_with_safety(id, safety))
                .await?;
            return Ok(Step::Again);
        }
        Ok(Step::Wait(format!(
            "等待节点安装成员变更: {}",
            waiting.join("；")
        )))
    }

    async fn deliver_finish(&self, op: &Operation, finish: &Finish, me: &str) -> io::Result<Step> {
        let proposal = finish.decision.proposal.clone().ok_or_else(denied)?;
        let mut waiting = Vec::new();
        for member in &proposal.destinations {
            if op.delivered.contains(&member.member_id)
                && (member.member_id != me || op.finished_locally)
            {
                continue;
            }
            let retained = proposal.desired.member(&member.member_id).is_some();
            let reply = if member.member_id == me {
                match self.apply_finish(finish.clone()).await {
                    Ok(()) => Reply::Ok(serde_json::Value::Null),
                    Err(error) => refusal_or_wait(&error),
                }
            } else {
                if !retained {
                    // A departing node may have missed the decision; it is
                    // idempotent and must precede its cleanup proof.
                    let _ = self
                        .call::<_, PreparedReceipt>(member, &op.intent, routes::PREPARE, &proposal)
                        .await;
                    let _ = self
                        .call::<_, Option<InstalledReceipt>>(
                            member,
                            &op.intent,
                            routes::DECISION,
                            &finish.decision,
                        )
                        .await;
                }
                self.call::<_, serde_json::Value>(member, &op.intent, routes::FINISH, finish)
                    .await
            };
            match reply {
                Reply::Ok(_) => {
                    let (id, member_id) =
                        (op.intent.operation_id.clone(), member.member_id.clone());
                    self.blocking(move |m| m.delivered(id, member_id)).await?;
                }
                Reply::Refused(message) | Reply::Unavailable(message) if retained => {
                    waiting.push(format!("{}: {message}", member.node_id))
                }
                _ => {}
            }
        }
        if waiting.is_empty() {
            if let Some(manifest) = self.membership.manifest()? {
                self.refresh_fencing_policies(&manifest).await;
            }
            Ok(Step::Done)
        } else {
            Ok(Step::Wait(format!(
                "等待节点确认完成: {}",
                waiting.join("；")
            )))
        }
    }

    async fn deliver_abort(&self, op: &Operation, me: &str) -> io::Result<Step> {
        let decision = op.decision.clone().ok_or_else(denied)?;
        let mut recipients = op.destinations();
        let mut waiting = Vec::new();
        if let (None, None, true, Change::Add { target_url }) = (
            &op.proposal,
            &op.pairing_candidate,
            op.pairing_contacted,
            &op.intent.change,
        ) {
            if let Ok(Ok(identity)) = tokio::time::timeout(
                PEER_TIMEOUT,
                self.client.discover(&member_endpoint(target_url)?),
            )
            .await
            {
                if !recipients.iter().any(|m| m.member_id == identity.member_id) {
                    let candidate = candidate_descriptor(&identity, target_url);
                    let (id, pinned) = (op.intent.operation_id.clone(), candidate.clone());
                    self.blocking(move |m| m.pin_candidate(id, pinned)).await?;
                    recipients.push(candidate);
                }
            } else {
                waiting.push("等待目标服务器恢复以清理可能的配对预约".into());
            }
        }
        for member in recipients {
            if op.delivered.contains(&member.member_id)
                && (member.member_id != me || op.finished_locally)
            {
                continue;
            }
            let reply = if member.member_id == me {
                match self.apply_decision(decision.clone()).await {
                    Ok(_) => Reply::Ok(None),
                    Err(error) => refusal_or_wait(&error),
                }
            } else {
                self.call::<_, Option<InstalledReceipt>>(
                    &member,
                    &op.intent,
                    routes::ABORT,
                    &decision,
                )
                .await
            };
            let candidate = op.intent.base.member(&member.member_id).is_none();
            match reply {
                Reply::Ok(_) => {}
                // A candidate without this reservation has nothing to release.
                Reply::Refused(_) if candidate => {}
                Reply::Refused(message) | Reply::Unavailable(message) => {
                    waiting.push(format!("{}: {message}", member.node_id));
                    continue;
                }
            }
            let (id, member_id) = (op.intent.operation_id.clone(), member.member_id.clone());
            self.blocking(move |m| m.delivered(id, member_id)).await?;
        }
        if waiting.is_empty() {
            Ok(Step::Done)
        } else {
            Ok(Step::Wait(format!(
                "等待节点确认取消: {}",
                waiting.join("；")
            )))
        }
    }

    // ----- participant -----

    pub async fn handle_prepare(
        &self,
        sender: &str,
        proposal: Proposal,
    ) -> io::Result<PreparedReceipt> {
        if sender != proposal.intent.coordinator {
            return Err(denied());
        }
        self.membership.prepare(proposal, || (self.stop)()).await
    }

    pub async fn handle_decision(
        &self,
        sender: &str,
        decision: Decision,
        kind: DecisionKind,
    ) -> io::Result<Option<InstalledReceipt>> {
        if sender != decision.intent.coordinator || decision.kind != kind {
            return Err(denied());
        }
        self.apply_decision(decision).await
    }

    pub async fn handle_finish(&self, sender: &str, finish: Finish) -> io::Result<()> {
        if sender != finish.decision.intent.coordinator {
            return Err(denied());
        }
        self.apply_finish(finish).await
    }

    pub async fn handle_status(
        &self,
        sender: &str,
        request: StatusRequest,
    ) -> io::Result<StatusReply> {
        let local = self.membership.local()?;
        if let Some(pairing) = local.pairing.filter(|p| p.intent == request.intent) {
            if sender != request.intent.coordinator {
                return Err(denied());
            }
            return Ok(StatusReply {
                pairing: Some(pairing.receipt),
                ..Default::default()
            });
        }
        let op = self
            .membership
            .operation(&request.intent.operation_id)?
            .filter(|op| op.intent == request.intent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "成员操作不存在"))?;
        if request.retry && op.intent.coordinator == self.me()? {
            self.spawn_driver(op.intent.operation_id.clone());
        }
        Ok(StatusReply {
            status: Some(op.status()),
            pairing: None,
            decision: op.decision.clone(),
            finish: op.finish.clone(),
        })
    }

    pub async fn handle_forward(
        &self,
        sender: &str,
        request: ForwardRequest,
    ) -> io::Result<OperationStatus> {
        let (id, sender) = (request.operation_id.clone(), sender.to_owned());
        let op = self
            .blocking(move |m| {
                let op = m.begin(
                    id.clone(),
                    request.expected_revision,
                    sender,
                    request.change,
                )?;
                m.propose(id, None)?;
                Ok(op)
            })
            .await?;
        self.spawn_driver(op.intent.operation_id.clone());
        Ok(self
            .membership
            .operation(&op.intent.operation_id)?
            .unwrap_or(op)
            .status())
    }

    /// Explicit create / prepare-to-join: hold first, real source shutdown,
    /// then the one-member manifest or the join-ready state.
    pub async fn setup(&self, request: SetupRequest, create: bool) -> io::Result<()> {
        self.membership
            .setup(request, create, || (self.stop)())
            .await?;
        self.membership_changed();
        Ok(())
    }

    pub async fn handle_reserve<F>(
        &self,
        intent: Intent,
        authorize: F,
    ) -> io::Result<PairingReceipt>
    where
        F: FnOnce(&crate::storage::Transaction<'_>) -> io::Result<()> + Send + 'static,
    {
        self.blocking(move |m| m.reserve_pairing(intent, authorize))
            .await
    }

    async fn apply_decision(&self, decision: Decision) -> io::Result<Option<InstalledReceipt>> {
        let _gate = MEMBERSHIP_GATE.write().await;
        let receipt = self.blocking(move |m| m.apply_decision(decision)).await?;
        self.membership_changed();
        Ok(receipt)
    }

    async fn apply_finish(&self, finish: Finish) -> io::Result<()> {
        self.blocking(move |m| m.apply_finish(finish)).await?;
        self.membership_changed();
        Ok(())
    }

    /// Old heartbeat and owner freshness never count under a new membership:
    /// execution resumes only after fresh signed agreement.
    fn membership_changed(&self) {
        if !self.process_hooks {
            return;
        }
        {
            let mut state = crate::cluster::cluster_state_write();
            state.peer_heartbeat_acks.clear();
            state.peer_heartbeat_observed.clear();
            state.peer_owner_views.clear();
            state.heartbeat_failures.clear();
            state.peer_observations.clear();
            state.nodes.retain(|_, node| node.is_local);
        }
        crate::plugins::set_config_updated();
        crate::webui::state::request_status_refresh();
        crate::cluster::notify_monitoring_changed();
    }

    /// Resume coordinated operations and ask coordinators for outcomes this
    /// participant has not yet learned. Never decides on its own.
    pub async fn recover(&self) -> io::Result<()> {
        let me = self.me()?;
        for op in self.membership.operations()? {
            if op.intent.coordinator == me {
                // Retained completion and remote cleanup are independent.
                // Keep retrying departed recipients after browser success.
                if !op.terminal() || op.cleanup_pending() || !op.finished_locally {
                    self.spawn_driver(op.intent.operation_id.clone());
                }
                continue;
            }
            if op.finished_locally {
                continue;
            }
            let Some(coordinator) = op.intent.base.member(&op.intent.coordinator).cloned() else {
                continue;
            };
            let request = StatusRequest {
                intent: op.intent.clone(),
                retry: false,
            };
            if let Reply::Ok(reply) = self
                .call::<_, StatusReply>(&coordinator, &op.intent, routes::STATUS, &request)
                .await
            {
                self.adopt(&op, reply).await;
            }
        }
        if let Some(manifest) = self.membership.manifest()? {
            self.refresh_fencing_policies(&manifest).await;
        }
        Ok(())
    }

    async fn adopt(&self, op: &Operation, reply: StatusReply) {
        let coordinator = &op.intent.coordinator;
        if let Some(decision) = reply.decision.filter(|d| d.intent == op.intent) {
            if op.installed.is_none() {
                let _ = self
                    .handle_decision(coordinator, decision.clone(), decision.kind)
                    .await;
            }
        }
        if let Some(finish) = reply.finish {
            let _ = self.handle_finish(coordinator, finish).await;
        }
    }

    /// A browser status/retry on a participant asks the recorded coordinator.
    pub async fn query_coordinator(&self, op: &Operation, retry: bool) -> Option<OperationStatus> {
        let coordinator = op.intent.base.member(&op.intent.coordinator)?.clone();
        let request = StatusRequest {
            intent: op.intent.clone(),
            retry,
        };
        match self
            .call::<_, StatusReply>(&coordinator, &op.intent, routes::STATUS, &request)
            .await
        {
            Reply::Ok(reply) => {
                let status = reply.status.clone();
                self.adopt(op, reply).await;
                status
            }
            _ => None,
        }
    }

    // ----- browser-initiated -----

    /// Every resulting member must answer an identity proof and a majority of
    /// the old membership must be reachable before an operation is recorded.
    pub async fn preflight(
        &self,
        base: &Manifest,
        desired: &[Descriptor],
    ) -> Result<(), Vec<String>> {
        let me = self.me().map_err(|e| vec![e.to_string()])?;
        let me = me.as_str();
        let checks = join_all(base.members.iter().map(|member| async move {
            if member.member_id == me {
                return (member, true);
            }
            let Ok(endpoint) = member_endpoint(&member.api_url) else {
                return (member, false);
            };
            let ok = tokio::time::timeout(
                Duration::from_secs(8),
                self.client.hello(&endpoint, &member.identity()),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            (member, ok)
        }))
        .await;
        let reachable = checks.iter().filter(|(_, ok)| *ok).count();
        let missing: Vec<String> = checks
            .iter()
            .filter(|(m, ok)| !ok && desired.iter().any(|d| d.member_id == m.member_id))
            .map(|(m, _)| m.node_id.clone())
            .collect();
        if !missing.is_empty() {
            return Err(missing);
        }
        if reachable <= base.members.len() / 2 {
            return Err(checks
                .iter()
                .filter(|(_, ok)| !ok)
                .map(|(m, _)| m.node_id.clone())
                .collect());
        }
        self.refresh_fencing_policies(base).await;
        Ok(())
    }

    /// Optional capabilities are acquired while a member is online and pinned
    /// to its key. Older binaries simply have no bounded-fencing authority.
    async fn refresh_fencing_policies(&self, manifest: &Manifest) {
        let Ok(me) = self.me() else {
            return;
        };
        for member in &manifest.members {
            if member.member_id == me {
                continue;
            }
            let Ok(response) = send_to_member(
                &self.membership,
                &self.client,
                member,
                &manifest.cluster_id,
                manifest.revision,
                PeerScope::Ordinary,
                routes::CAPABILITIES,
                vec![],
                PEER_TIMEOUT,
            )
            .await
            else {
                continue;
            };
            if response.status != 200 {
                continue;
            }
            let policy = serde_json::from_slice::<serde_json::Value>(&response.body)
                .ok()
                .and_then(|value| {
                    serde_json::from_value::<FencingPolicy>(value["membership_fencing"].clone())
                        .ok()
                });
            if let Some(policy) = policy {
                let identity = member.identity();
                let _ = self
                    .blocking(move |m| m.remember_fencing_policy(identity, policy))
                    .await;
            }
        }
    }

    /// Records intent before the only password-bearing request.
    pub async fn reserve_remote(
        &self,
        op: &Operation,
        password: String,
    ) -> io::Result<(PublicIdentity, Reservation)> {
        let Change::Add { target_url } = &op.intent.change else {
            return Err(denied());
        };
        let endpoint = member_endpoint(target_url)?;
        let identity = tokio::time::timeout(PEER_TIMEOUT, self.client.discover(&endpoint))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "目标服务器连接超时"))??;
        let (id, candidate) = (
            op.intent.operation_id.clone(),
            candidate_descriptor(&identity, target_url),
        );
        self.blocking(move |m| m.pin_candidate(id, candidate))
            .await?;
        #[derive(Serialize)]
        struct Body<'a> {
            intent: &'a Intent,
            password: &'a str,
        }
        let body = zeroize::Zeroizing::new(encode(&Body {
            intent: &op.intent,
            password: &password,
        })?);
        drop(zeroize::Zeroizing::new(password));
        let response = match tokio::time::timeout(
            PEER_TIMEOUT,
            self.client.reserve_pairing(&endpoint, body.to_vec()),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Ok((identity, Reservation::Uncertain(error.to_string()))),
            Err(_) => return Ok((identity, Reservation::Uncertain("配对请求超时".into()))),
        };
        if response.status != 200 {
            let message = serde_json::from_slice::<serde_json::Value>(&response.body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "目标服务器拒绝配对".into());
            // A proxy/origin failure may replace the reply after the target
            // durably reserved. Only application refusal statuses are definite.
            let reservation = if matches!(response.status, 400 | 403 | 409 | 429) {
                Reservation::Refused(response.status, message)
            } else {
                Reservation::Uncertain(format!("HTTP {}: {message}", response.status))
            };
            return Ok((identity, reservation));
        }
        let receipt: PairingReceipt = serde_json::from_slice(&response.body)
            .map_err(|_| invalid("目标服务器配对回执无效"))?;
        if receipt.candidate.member_id != identity.member_id || receipt.verify(&op.intent).is_err()
        {
            return Err(invalid("目标服务器配对回执与其身份不符"));
        }
        Ok((identity, Reservation::Reserved(receipt)))
    }

    /// Ask every other member whether this exact key is still enrolled. Local
    /// departure is one later transaction, and only when every answer is a
    /// signed absence proof. Anything else leaves the store unchanged.
    pub async fn leave_unrecognized(&self) -> io::Result<()> {
        let plan = self
            .blocking(|membership| membership.departure_plan())
            .await?;
        let cluster_id = plan.cluster_id.clone();
        let revision = plan.revision;
        let self_key = plan.self_key.clone();
        let expected = plan.others.len();
        let probes = join_all(plan.others.into_iter().map(|member| {
            let membership = self.membership.clone();
            let client = Arc::clone(&self.client);
            let cluster_id = cluster_id.clone();
            let self_key = self_key.clone();
            async move {
                let outcome = probe_member(
                    &membership,
                    &client,
                    &member,
                    &cluster_id,
                    revision,
                    &self_key,
                )
                .await;
                (member, outcome)
            }
        }))
        .await;
        let mut present = Vec::new();
        let mut missing = Vec::new();
        let mut proved = BTreeSet::new();
        for (member, outcome) in probes {
            match outcome {
                ProbeOutcome::Absent => {
                    proved.insert(member.member_id);
                }
                ProbeOutcome::Present => present.push(member_label(&member)),
                ProbeOutcome::NoProof => missing.push(member_label(&member)),
            }
        }
        present.sort();
        missing.sort();
        if !present.is_empty() || !missing.is_empty() || proved.len() != expected {
            return Err(conflict(&departure_failure(&present, &missing)));
        }
        self.blocking(move |membership| membership.commit_local_departure(proved))
            .await?;
        self.membership_changed();
        tracing::info!("本节点已确认其他服务器不再承认当前身份，退出集群并关闭监控");
        Ok(())
    }
}

enum ProbeOutcome {
    Absent,
    Present,
    NoProof,
}

fn member_label(member: &Descriptor) -> String {
    if member.name.is_empty() {
        member.node_id.clone()
    } else {
        format!("{}（{}）", member.name, member.node_id)
    }
}

fn departure_failure(present: &[String], missing: &[String]) -> String {
    let mut parts = Vec::new();
    if !present.is_empty() {
        parts.push(format!(
            "服务器 {} 仍承认本节点，请使用「移除」",
            present.join("、")
        ));
    }
    if !missing.is_empty() {
        parts.push(format!(
            "服务器 {} 没有确认本节点已不在集群中",
            missing.join("、")
        ));
    }
    if parts.is_empty() {
        "没有收到全部服务器的退出证明".into()
    } else {
        parts.join("。")
    }
}

async fn probe_member(
    membership: &Membership,
    client: &PeerClient,
    member: &Descriptor,
    cluster_id: &str,
    revision: u64,
    self_key: &PublicIdentity,
) -> ProbeOutcome {
    let probe = RecognitionProbe {
        cluster_id: cluster_id.to_owned(),
        member_id: self_key.member_id.clone(),
        public_key: self_key.public_key.clone(),
    };
    let Ok(body) = serde_json::to_vec(&probe) else {
        return ProbeOutcome::NoProof;
    };
    let Ok(response) = send_to_member(
        membership,
        client,
        member,
        cluster_id,
        revision,
        PeerScope::Ordinary,
        routes::RECOGNITION,
        body,
        PEER_TIMEOUT,
    )
    .await
    else {
        return ProbeOutcome::NoProof;
    };
    classify_recognition(&response, member, &probe)
}

/// A 200 body is an absence proof only when the pinned peer key signed
/// `absent` for this cluster and this exact key. Unsigned or substituted
/// bytes are not a proof.
fn classify_recognition(
    response: &PeerResponse,
    member: &Descriptor,
    probe: &RecognitionProbe,
) -> ProbeOutcome {
    if response.status != 200 {
        return ProbeOutcome::NoProof;
    }
    let Ok(answer) = serde_json::from_slice::<RecognitionAnswer>(&response.body) else {
        return ProbeOutcome::NoProof;
    };
    if answer.cluster_id != probe.cluster_id
        || answer.member_id != probe.member_id
        || answer.verify(&member.identity()).is_err()
    {
        return ProbeOutcome::NoProof;
    }
    match answer.verdict {
        RecognitionVerdict::Absent => ProbeOutcome::Absent,
        RecognitionVerdict::Present => ProbeOutcome::Present,
    }
}

fn candidate_descriptor(identity: &PublicIdentity, url: &str) -> Descriptor {
    Descriptor {
        member_id: identity.member_id.clone(),
        public_key: identity.public_key.clone(),
        node_id: "candidate".into(),
        name: String::new(),
        api_url: url.into(),
        priority: 0,
    }
}

fn refusal_or_wait<T>(error: &io::Error) -> Reply<T> {
    match error.kind() {
        io::ErrorKind::WouldBlock
        | io::ErrorKind::PermissionDenied
        | io::ErrorKind::InvalidInput => Reply::Refused(error.to_string()),
        _ => Reply::Unavailable(error.to_string()),
    }
}

fn running() -> &'static Mutex<HashSet<String>> {
    static RUNNING: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

fn process_fence_waits() -> Arc<Mutex<HashMap<String, tokio::time::Instant>>> {
    static WAITS: std::sync::OnceLock<Arc<Mutex<HashMap<String, tokio::time::Instant>>>> =
        std::sync::OnceLock::new();
    Arc::clone(WAITS.get_or_init(|| Arc::new(Mutex::new(HashMap::new()))))
}

/// Body intent for exact-operation routes. Parsed only to select the trust
/// grant, after the route's byte cap and before authentication.
pub fn operation_intent(path: &str, body: &[u8]) -> io::Result<Intent> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| invalid("成员操作请求格式无效"))?;
    let intent = if path == routes::FINISH.path {
        &value["decision"]["intent"]
    } else {
        &value["intent"]
    };
    decode(intent.clone())
}

/// Stops every source process through the existing supervisor and confirms
/// cessation before a prepared receipt may be signed.
pub async fn stop_local_execution() -> io::Result<()> {
    crate::plugins::set_manual_restart();
    crate::cluster::clear_local_stream();
    crate::plugins::stop_ffmpeg().await;
    crate::plugins::enable_danmaku_commands(false);
    if crate::plugins::is_danmaku_running() {
        crate::plugins::stop_danmaku().await;
    }
    if crate::plugins::is_ffmpeg_running().await {
        return Err(io::Error::other("本节点推流尚未停止"));
    }
    Ok(())
}

/// Fails closed: an unreadable Store is treated as held.
pub fn current_lifecycle() -> Lifecycle {
    crate::storage::global()
        .and_then(|store| Membership::new(store).local())
        .map(|local| local.lifecycle)
        .unwrap_or(Lifecycle::IncompatibleHeld)
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static HOLD_STORE: Arc<Store>;
}

/// Durable maintenance, join, departure and legacy holds. Checked before the
/// `cluster.enabled` shortcut in monitoring fences.
pub fn membership_block_reason() -> Option<String> {
    #[cfg(test)]
    let store = match HOLD_STORE.try_with(Arc::clone) {
        Ok(store) => store,
        Err(_) => return None,
    };
    #[cfg(not(test))]
    let store = match crate::storage::global() {
        Ok(store) => store,
        Err(_) => return Some("集群成员状态无法读取，暂停本节点执行".into()),
    };
    hold_reason(&store)
}

pub fn hold_reason(store: &Store) -> Option<String> {
    execution_hold(store).unwrap_or_else(|_| Some("集群成员状态无法读取，暂停本节点执行".into()))
}

pub struct RecoveryWorker(tokio::task::JoinHandle<()>);

impl Drop for RecoveryWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Background resume after restart and for participants waiting on an
/// outcome. One worker per process, started with the admin server.
pub fn start_recovery_worker() -> RecoveryWorker {
    RecoveryWorker(tokio::spawn(async {
        loop {
            if let Ok(runtime) = Runtime::process() {
                if runtime.membership.identity().is_ok() {
                    if let Err(error) = runtime.recover().await {
                        tracing::debug!("成员操作恢复检查失败: {error}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    }))
}
