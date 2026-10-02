use super::*;
use crate::cluster::peer_auth::{
    Domain, NodeIdentity, PeerScope, PublicIdentity, RecognitionProbe, RecognitionVerdict,
    TrustSnapshot,
};
use crate::storage::{Store, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io,
    sync::Arc,
};

pub const LOCAL_RECORD: &str = "cluster-local";
pub const MANIFEST_RECORD: &str = "cluster-membership";
pub const OPERATIONS_RECORD: &str = "cluster-operations";
const MAX_OPERATIONS: usize = 128;

// Older journals predate pinning and may have sent the password already.
fn unknown_pairing_contact() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Standalone,
    IncompatibleHeld,
    JoinReady,
    Pairing,
    Managed,
    Left,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingReservation {
    pub intent: Intent,
    pub receipt: PairingReceipt,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalState {
    pub version: u32,
    pub lifecycle: Lifecycle,
    pub descriptor: Option<Descriptor>,
    pub hold: Option<String>,
    pub reservation: Option<String>,
    pub pairing: Option<PairingReservation>,
    pub setup_operation: Option<String>,
    pub setup_intent: Option<String>,
    #[serde(default)]
    pub removed_sources: BTreeMap<String, Finish>,
}
impl Default for LocalState {
    fn default() -> Self {
        Self {
            version: PROTOCOL,
            lifecycle: Lifecycle::Standalone,
            descriptor: None,
            hold: None,
            reservation: None,
            pairing: None,
            setup_operation: None,
            setup_intent: None,
            removed_sources: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub intent: Intent,
    pub proposal: Option<Proposal>,
    pub prepared: Option<PreparedReceipt>,
    pub decision: Option<Decision>,
    pub installed: Option<InstalledReceipt>,
    pub finish: Option<Finish>,
    pub prepares: BTreeMap<String, PreparedReceipt>,
    pub installations: BTreeMap<String, InstalledReceipt>,
    pub delivered: BTreeSet<String>,
    pub finished_locally: bool,
    pub message: Option<String>,
    /// Pinned before the password-bearing request, even if its reply is lost.
    #[serde(default)]
    pub pairing_candidate: Option<Descriptor>,
    #[serde(default = "unknown_pairing_contact")]
    pub pairing_contacted: bool,
    #[serde(default)]
    pub dissolved: bool,
}
impl Operation {
    fn new(intent: Intent) -> Self {
        Self {
            intent,
            proposal: None,
            prepared: None,
            decision: None,
            installed: None,
            finish: None,
            prepares: BTreeMap::new(),
            installations: BTreeMap::new(),
            delivered: BTreeSet::new(),
            finished_locally: false,
            message: None,
            pairing_candidate: None,
            pairing_contacted: false,
            dissolved: false,
        }
    }
    pub fn terminal(&self) -> bool {
        self.dissolved
            || self.finish.is_some()
            || self
                .decision
                .as_ref()
                .is_some_and(|d| d.kind == DecisionKind::Abort)
    }
    pub fn cleanup_pending(&self) -> bool {
        if !self.terminal() || self.dissolved {
            return false;
        }
        self.destinations()
            .iter()
            .any(|m| !self.delivered.contains(&m.member_id))
            || (matches!(self.intent.change, Change::Add { .. })
                && self.pairing_contacted
                && self.proposal.is_none()
                && self.pairing_candidate.is_none())
    }
    pub(super) fn destinations(&self) -> Vec<Descriptor> {
        let mut members = self
            .proposal
            .as_ref()
            .map(|p| p.destinations.clone())
            .unwrap_or_else(|| self.intent.base.members.clone());
        if let Some(candidate) = &self.pairing_candidate {
            if !members.iter().any(|m| m.member_id == candidate.member_id) {
                members.push(candidate.clone());
            }
        }
        members
    }
    pub fn status(&self) -> OperationStatus {
        let phase = if self.finish.is_some() || self.dissolved {
            "completed"
        } else {
            match self.decision.as_ref().map(|d| d.kind) {
                Some(DecisionKind::Abort) => {
                    if self.finished_locally {
                        "aborted"
                    } else {
                        "aborting"
                    }
                }
                Some(DecisionKind::Commit) => "committing",
                None => "preparing",
            }
        };
        let aborting = self
            .decision
            .as_ref()
            .is_some_and(|d| d.kind == DecisionKind::Abort);
        let pending = if aborting {
            let mut pending: Vec<String> = self
                .destinations()
                .iter()
                .filter(|m| !self.delivered.contains(&m.member_id))
                .map(|m| m.node_id.clone())
                .collect();
            if matches!(self.intent.change, Change::Add { .. })
                && self.pairing_contacted
                && self.proposal.is_none()
                && self.pairing_candidate.is_none()
            {
                pending.push("配对目标（预约待核对）".into());
            }
            pending
        } else {
            self.proposal
                .as_ref()
                .map(|p| {
                    p.desired
                        .members
                        .iter()
                        .filter(|m| {
                            if self.finish.is_some() {
                                !self.delivered.contains(&m.member_id)
                            } else if self.decision.is_some() {
                                !self.installations.contains_key(&m.member_id)
                            } else {
                                !self.prepares.contains_key(&m.member_id)
                            }
                        })
                        .map(|m| m.node_id.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        OperationStatus {
            operation_id: self.intent.operation_id.clone(),
            kind: self.intent.change.kind().into(),
            phase: phase.into(),
            coordinator_node_id: self
                .intent
                .base
                .member(&self.intent.coordinator)
                .map(|m| m.node_id.clone())
                .unwrap_or_default(),
            message: self.message.clone(),
            pending_node_ids: pending,
            cleanup_pending_node_ids: if self.finish.is_some() {
                self.destinations()
                    .iter()
                    .filter(|m| {
                        !self.delivered.contains(&m.member_id)
                            && self
                                .proposal
                                .as_ref()
                                .is_some_and(|p| p.desired.member(&m.member_id).is_none())
                    })
                    .map(|m| m.node_id.clone())
                    .collect()
            } else {
                Vec::new()
            },
            terminal: self.terminal(),
            retryable: !self.finished_locally || self.cleanup_pending() || !self.terminal(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationStatus {
    pub operation_id: String,
    pub kind: String,
    pub phase: String,
    pub coordinator_node_id: String,
    pub message: Option<String>,
    pub pending_node_ids: Vec<String>,
    #[serde(default)]
    pub cleanup_pending_node_ids: Vec<String>,
    pub terminal: bool,
    pub retryable: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Journal {
    sequence: u64,
    operations: BTreeMap<String, Operation>,
    #[serde(default)]
    fencing_policies: BTreeMap<String, FencingPolicy>,
}

#[derive(Clone)]
pub struct Membership {
    store: Arc<Store>,
}
impl Membership {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }
    pub fn identity(&self) -> io::Result<NodeIdentity> {
        NodeIdentity::load(&self.store)?.ok_or_else(|| invalid("本地节点身份缺失"))
    }
    pub fn manifest(&self) -> io::Result<Option<Manifest>> {
        read(&self.store, MANIFEST_RECORD)
    }
    pub fn local(&self) -> io::Result<LocalState> {
        Ok(read(&self.store, LOCAL_RECORD)?.unwrap_or_default())
    }
    pub fn operation(&self, id: &str) -> io::Result<Option<Operation>> {
        validate_operation_id(id)?;
        Ok(read::<Journal>(&self.store, OPERATIONS_RECORD)?
            .unwrap_or_default()
            .operations
            .remove(id))
    }
    pub fn operations(&self) -> io::Result<Vec<Operation>> {
        Ok(read::<Journal>(&self.store, OPERATIONS_RECORD)?
            .unwrap_or_default()
            .operations
            .into_values()
            .collect())
    }
    pub fn remember_fencing_policy(
        &self,
        member: PublicIdentity,
        policy: FencingPolicy,
    ) -> io::Result<()> {
        policy.verify(&member)?;
        self.store.transaction(move |tx| {
            let manifest: Manifest = read_tx(tx, MANIFEST_RECORD)?.ok_or_else(denied)?;
            if manifest
                .member(&member.member_id)
                .is_none_or(|m| m.identity() != member)
            {
                return Err(denied());
            }
            let mut journal = journal_tx(tx)?;
            if journal.fencing_policies.get(&member.member_id) == Some(&policy) {
                return Ok(());
            }
            let pending_members: BTreeSet<String> = journal
                .operations
                .values()
                .filter(|op| !op.terminal())
                .flat_map(|op| op.intent.base.members.iter().map(|m| m.member_id.clone()))
                .collect();
            journal
                .fencing_policies
                .retain(|id, _| manifest.member(id).is_some() || pending_members.contains(id));
            journal.fencing_policies.insert(member.member_id, policy);
            write_tx(tx, OPERATIONS_RECORD, &journal)
        })
    }
    pub fn fencing_policy(&self, member: &Descriptor) -> io::Result<Option<FencingPolicy>> {
        let journal: Journal = read(&self.store, OPERATIONS_RECORD)?.unwrap_or_default();
        let policy = journal.fencing_policies.get(&member.member_id).cloned();
        if let Some(policy) = &policy {
            policy.verify(&member.identity())?;
        }
        Ok(policy)
    }

    /// Stop evidence survives journal compaction and benign later membership
    /// edits. A current member with the same friendly id always invalidates it.
    pub fn removed_source_stopped(&self, node_id: &str) -> io::Result<bool> {
        let docs = self.store.read_many(&[LOCAL_RECORD, MANIFEST_RECORD])?;
        let local: LocalState = decode(docs.get(LOCAL_RECORD).ok_or_else(denied)?.value.clone())?;
        let current: Manifest =
            decode(docs.get(MANIFEST_RECORD).ok_or_else(denied)?.value.clone())?;
        if local.lifecycle != Lifecycle::Managed
            || local.hold.is_some()
            || current.members.iter().any(|m| m.node_id == node_id)
        {
            return Ok(false);
        }
        let Some(finish) = local.removed_sources.get(node_id) else {
            return Ok(false);
        };
        finish.validate()?;
        finish.validate_removal_safety()?;
        let p = finish.decision.proposal.as_ref().ok_or_else(denied)?;
        Ok(p.desired.cluster_id == current.cluster_id
            && p.desired.revision <= current.revision
            && matches!(&p.intent.change, Change::Remove { target_member_id, .. }
                if p.intent.base.member(target_member_id).is_some_and(|m| m.node_id == node_id)))
    }

    /// Called before any runtime starts. A legacy enabled topology never becomes
    /// an implicitly standalone process; missing managed keys are a hard error.
    pub fn initialize(&self) -> io::Result<()> {
        self.store.transaction(|tx| {
            let local: Option<LocalState> = read_tx(tx, LOCAL_RECORD)?;
            let manifest: Option<Manifest> = read_tx(tx, MANIFEST_RECORD)?;
            if let Some(state) = local {
                if state.version != PROTOCOL {
                    return Err(invalid("成员状态版本不兼容"));
                }
                if !matches!(
                    state.lifecycle,
                    Lifecycle::IncompatibleHeld | Lifecycle::Standalone
                ) {
                    let identity = NodeIdentity::load_from(tx)?
                        .ok_or_else(|| invalid("受保护节点身份缺失"))?;
                    if state
                        .descriptor
                        .as_ref()
                        .is_none_or(|d| d.member_id != identity.public().member_id)
                    {
                        return Err(invalid("节点身份与本地状态不匹配"));
                    }
                }
                if state.lifecycle == Lifecycle::Managed {
                    let manifest = manifest.ok_or_else(|| invalid("成员清单缺失"))?;
                    manifest.validate()?;
                    // A departing node installs the successor manifest and
                    // stays held until FINISH; it must still start to get it.
                    if state.descriptor.is_none()
                        || (state.hold.is_none()
                            && state
                                .descriptor
                                .as_ref()
                                .is_some_and(|d| manifest.member(&d.member_id).is_none()))
                    {
                        return Err(invalid("本地身份已不在成员清单"));
                    }
                }
            } else if manifest.is_some() {
                return Err(invalid("成员本地状态缺失"));
            } else if tx
                .read("config.json")?
                .is_some_and(|d| d.value["cluster"]["enabled"] == true)
            {
                write_tx(
                    tx,
                    LOCAL_RECORD,
                    &LocalState {
                        lifecycle: Lifecycle::IncompatibleHeld,
                        hold: Some("incompatible".into()),
                        ..LocalState::default()
                    },
                )?;
            }
            Ok(())
        })
    }

    /// Explicit setup persists its hold before invoking the real supervisor.
    /// Returning from stop must mean every source/ffmpeg process is stopped.
    pub async fn setup<F, Fut>(
        &self,
        request: SetupRequest,
        create: bool,
        stop: F,
    ) -> io::Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let this = self.clone();
        let req = request.clone();
        tokio::task::spawn_blocking(move || this.begin_setup(req, create))
            .await
            .map_err(join_error)??;
        stop().await?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.finish_setup(request, create))
            .await
            .map_err(join_error)?
    }
    fn begin_setup(&self, req: SetupRequest, create: bool) -> io::Result<()> {
        validate_operation_id(&req.operation_id)?;
        self.store.transaction(move |tx| {
            let current = tx.read(LOCAL_RECORD)?;
            let revision = current.as_ref().map(|d| d.revision).unwrap_or(0);
            let mut local: LocalState = current
                .map(|d| decode(d.value))
                .transpose()?
                .unwrap_or_default();
            let intent = crate::cluster::peer_auth::digest_hex(&encode(&(create, &req))?);
            if local.setup_operation.as_deref() == Some(&req.operation_id) {
                return if local.setup_intent.as_deref() == Some(&intent) {
                    Ok(())
                } else {
                    Err(conflict("同一操作标识不能更改内容"))
                };
            }
            if revision != req.expected_local_revision {
                return Err(conflict("本地状态已改变，请刷新"));
            }
            if matches!(local.lifecycle, Lifecycle::Managed | Lifecycle::Pairing)
                || local.reservation.is_some()
            {
                return Err(conflict("已有成员操作，不能覆盖"));
            }
            let identity = if local.lifecycle == Lifecycle::Left {
                NodeIdentity::rotate_left_in(tx)?
            } else {
                NodeIdentity::create_in(tx)?
            };
            let descriptor = Descriptor {
                member_id: identity.public().member_id.clone(),
                public_key: identity.public().public_key.clone(),
                node_id: req.node_id,
                name: req.name,
                api_url: req.api_url,
                priority: req.priority,
            };
            descriptor.validate()?;
            local.descriptor = Some(descriptor);
            local.lifecycle = Lifecycle::JoinReady;
            local.hold = Some(req.operation_id.clone());
            local.setup_operation = Some(req.operation_id);
            local.setup_intent = Some(intent);
            write_tx(tx, LOCAL_RECORD, &local)
        })
    }
    fn finish_setup(&self, req: SetupRequest, create: bool) -> io::Result<()> {
        self.store.transaction(move |tx| {
            let mut local = local_tx(tx)?;
            if local.setup_operation.as_deref() != Some(&req.operation_id)
                || local.setup_intent.as_deref()
                    != Some(&crate::cluster::peer_auth::digest_hex(&encode(&(
                        create, &req,
                    ))?))
            {
                return Err(conflict("本地设置操作已改变"));
            }
            if create && local.lifecycle == Lifecycle::Managed {
                return Ok(());
            }
            if local.lifecycle != Lifecycle::JoinReady
                || local.reservation.is_some()
                || local.pairing.is_some()
            {
                return Err(conflict("本地设置操作已改变"));
            }
            if create {
                let descriptor = local.descriptor.clone().ok_or_else(denied)?;
                let manifest =
                    Manifest::new(req.operation_id.clone(), 1, vec![descriptor.clone()], None)?;
                project_config(tx, Some(&manifest), &descriptor.member_id, false)?;
                write_tx(tx, MANIFEST_RECORD, &manifest)?;
                local.lifecycle = Lifecycle::Managed;
                local.hold = None;
            } else {
                local.hold = Some("join_ready".into());
            }
            write_tx(tx, LOCAL_RECORD, &local)
        })
    }

    /// Password authentication and revision recheck are supplied by the auth
    /// layer and execute in this same transaction, before inspecting state.
    pub fn reserve_pairing<F>(&self, intent: Intent, authorize: F) -> io::Result<PairingReceipt>
    where
        F: FnOnce(&Transaction<'_>) -> io::Result<()> + Send + 'static,
    {
        intent.validate()?;
        if !matches!(intent.change, Change::Add { .. }) {
            return Err(denied());
        }
        self.store.transaction(move |tx| {
            authorize(tx)?;
            let mut local = local_tx(tx)?;
            let journal = journal_tx(tx)?;
            if journal
                .operations
                .get(&intent.operation_id)
                .is_some_and(|o| {
                    o.decision
                        .as_ref()
                        .is_some_and(|d| d.kind == DecisionKind::Abort)
                })
            {
                return Err(conflict("配对操作已取消"));
            }
            if let Some(existing) = &local.pairing {
                return if existing.intent == intent {
                    Ok(existing.receipt.clone())
                } else {
                    Err(conflict("本节点已被另一操作预约"))
                };
            }
            if local.lifecycle != Lifecycle::JoinReady
                || local.hold.as_deref() != Some("join_ready")
            {
                return Err(conflict("请先在目标服务器准备加入集群"));
            }
            let identity = identity_tx(tx)?;
            let receipt = PairingReceipt::sign(
                &intent,
                local.descriptor.clone().ok_or_else(denied)?,
                &identity,
            )?;
            receipt.verify(&intent)?;
            desired_manifest(&intent, Some(&receipt))?;
            local.lifecycle = Lifecycle::Pairing;
            local.hold = Some(intent.operation_id.clone());
            local.pairing = Some(PairingReservation {
                intent,
                receipt: receipt.clone(),
            });
            write_tx(tx, LOCAL_RECORD, &local)?;
            Ok(receipt)
        })
    }

    pub fn pairing_receipt(&self, intent: &Intent) -> io::Result<PairingReceipt> {
        let local = self.local()?;
        let pairing = local.pairing.ok_or_else(denied)?;
        if pairing.intent != *intent {
            return Err(denied());
        }
        Ok(pairing.receipt)
    }

    /// For forwarding, initiator is the authenticated original member. The
    /// selected coordinator must be retained, never a permanent cluster role.
    pub fn begin(
        &self,
        id: String,
        expected_revision: u64,
        initiator: String,
        change: Change,
    ) -> io::Result<Operation> {
        validate_operation_id(&id)?;
        self.store.transaction(move |tx| {
            let identity = identity_tx(tx)?;
            let base: Manifest = read_tx(tx, MANIFEST_RECORD)?.ok_or_else(denied)?;
            let mut journal = journal_tx(tx)?;
            if let Some(existing) = journal.operations.get(&id) {
                if existing.intent.base.revision == expected_revision
                    && existing.intent.initiator == initiator
                    && existing.intent.change == change
                    && existing.intent.coordinator == identity.public().member_id
                {
                    return Ok(existing.clone());
                }
                return Err(conflict("同一操作标识不能更改内容"));
            }
            let local = local_tx(tx)?;
            if local.lifecycle != Lifecycle::Managed
                || local.hold.is_some()
                || local.reservation.is_some()
                || base.revision != expected_revision
                || journal.operations.values().any(|o| !o.terminal())
            {
                return Err(conflict("成员版本已改变或已有待恢复操作"));
            }
            make_room(&mut journal, base.revision, &identity.public().member_id)?;
            journal.sequence = journal
                .sequence
                .checked_add(1)
                .ok_or_else(|| invalid("操作序列已达上限"))?;
            let intent = Intent {
                version: PROTOCOL,
                operation_id: id.clone(),
                sequence: journal.sequence,
                coordinator: identity.public().member_id.clone(),
                initiator,
                base,
                change,
            };
            validate_change(&intent)?;
            let operation = Operation::new(intent);
            journal.operations.insert(id, operation.clone());
            write_tx(tx, OPERATIONS_RECORD, &journal)?;
            Ok(operation)
        })
    }
    pub fn propose(&self, id: String, pairing: Option<PairingReceipt>) -> io::Result<Proposal> {
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
            ensure_coordinator(tx, op)?;
            if let Some(proposal) = &op.proposal {
                if proposal.pairing == pairing {
                    return Ok(Ok(proposal.clone()));
                }
                return Err(conflict("操作内容已确定"));
            }
            if op.decision.is_some() {
                return Err(conflict("操作已决定"));
            }
            let identity = identity_tx(tx)?;
            let proposal = match Proposal::build(op.intent.clone(), pairing.clone(), &identity) {
                Ok(proposal) => proposal,
                Err(error) => {
                    // Commit the abort even though proposal construction failed.
                    // Returning an error inside the transaction would roll it back
                    // and strand a password-established candidate reservation.
                    if let Some(receipt) = pairing.filter(|r| r.verify(&op.intent).is_ok()) {
                        op.pairing_candidate = Some(receipt.candidate);
                    }
                    op.decision = Some(Decision::sign(
                        op.intent.clone(),
                        None,
                        DecisionKind::Abort,
                        vec![],
                        &identity,
                    )?);
                    op.message = Some(error.to_string());
                    write_tx(tx, OPERATIONS_RECORD, &journal)?;
                    return Ok(Err(error));
                }
            };
            op.proposal = Some(proposal.clone());
            write_tx(tx, OPERATIONS_RECORD, &journal)?;
            Ok(Ok(proposal))
        })?
    }

    pub fn pin_candidate(&self, id: String, candidate: Descriptor) -> io::Result<()> {
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
            ensure_coordinator(tx, op)?;
            candidate.validate()?;
            if op
                .pairing_candidate
                .as_ref()
                .is_some_and(|old| old.identity() != candidate.identity())
            {
                return Err(conflict("目标服务器身份已改变，仍需清理原预约"));
            }
            op.pairing_candidate = Some(candidate);
            op.pairing_contacted = true;
            write_tx(tx, OPERATIONS_RECORD, &journal)
        })
    }

    /// The final member leaves locally: durable hold, verified shutdown, then
    /// one atomic monitors-off/Left transition. No quorum certificate is made.
    pub async fn dissolve<F, Fut>(&self, id: String, stop: F) -> io::Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let this = self.clone();
        let operation = id.clone();
        let done = tokio::task::spawn_blocking(move || {
            this.store.transaction(move |tx| {
                let journal = journal_tx(tx)?;
                let op = journal.operations.get(&operation).ok_or_else(denied)?;
                ensure_coordinator(tx, op)?;
                if !op.intent.is_dissolve() {
                    return Err(denied());
                }
                if op.dissolved {
                    return Ok(true);
                }
                let mut local = local_tx(tx)?;
                admit_initial(tx, &local, &op.intent)?;
                if local.hold.as_ref().is_some_and(|hold| hold != &operation) {
                    return Err(conflict("已有另一项成员维护"));
                }
                local.hold = Some(operation.clone());
                local.reservation = Some(operation);
                write_tx(tx, LOCAL_RECORD, &local)?;
                Ok(false)
            })
        })
        .await
        .map_err(join_error)??;
        if done {
            return Ok(());
        }
        stop().await?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.store.transaction(move |tx| {
                let mut local = local_tx(tx)?;
                let mut journal = journal_tx(tx)?;
                let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
                ensure_coordinator(tx, op)?;
                if op.dissolved {
                    return Ok(());
                }
                if !op.intent.is_dissolve()
                    || local.hold.as_deref() != Some(&id)
                    || local.reservation.as_deref() != Some(&id)
                {
                    return Err(denied());
                }
                project_config(tx, None, &op.intent.coordinator, true)?;
                release_matching(&mut local, &id, true);
                op.dissolved = true;
                op.finished_locally = true;
                write_tx(tx, LOCAL_RECORD, &local)?;
                write_tx(tx, OPERATIONS_RECORD, &journal)
            })
        })
        .await
        .map_err(join_error)?
    }

    pub async fn prepare<F, Fut>(&self, proposal: Proposal, stop: F) -> io::Result<PreparedReceipt>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let this = self.clone();
        let p = proposal.clone();
        let existing = tokio::task::spawn_blocking(move || this.begin_prepare(p))
            .await
            .map_err(join_error)??;
        if let Some(receipt) = existing {
            return Ok(receipt);
        }
        stop().await?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.complete_prepare(proposal))
            .await
            .map_err(join_error)?
    }
    pub(crate) fn begin_prepare(&self, proposal: Proposal) -> io::Result<Option<PreparedReceipt>> {
        proposal.validate()?;
        self.store.transaction(move |tx| {
            let mut local = local_tx(tx)?;
            let mut journal = journal_tx(tx)?;
            check_existing(&journal, &proposal.intent, Some(&proposal))?;
            if let Some(op) = journal.operations.get(&proposal.intent.operation_id) {
                if op
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.kind == DecisionKind::Abort)
                {
                    return Err(conflict("操作已取消，拒绝延迟准备"));
                }
                if let Some(receipt) = &op.prepared {
                    return Ok(Some(receipt.clone()));
                }
            }
            admit_initial(tx, &local, &proposal.intent)?;
            let id = identity_tx(tx)?.public().member_id.clone();
            if proposal.member(&id).is_none() {
                return Err(denied());
            }
            if let Some(p) = &local.pairing {
                if proposal.pairing.as_ref() != Some(&p.receipt) {
                    return Err(denied());
                }
            }
            if local
                .reservation
                .as_ref()
                .is_some_and(|id| id != &proposal.intent.operation_id)
            {
                return Err(conflict("另一成员操作已取得本节点预约"));
            }
            make_room(&mut journal, proposal.intent.base.revision, &id)?;
            let op = journal
                .operations
                .entry(proposal.intent.operation_id.clone())
                .or_insert_with(|| Operation::new(proposal.intent.clone()));
            op.proposal = Some(proposal.clone());
            local.reservation = Some(proposal.intent.operation_id.clone());
            local.hold = local.reservation.clone();
            write_tx(tx, LOCAL_RECORD, &local)?;
            write_tx(tx, OPERATIONS_RECORD, &journal)?;
            Ok(None)
        })
    }
    pub(crate) fn complete_prepare(&self, proposal: Proposal) -> io::Result<PreparedReceipt> {
        self.store.transaction(move |tx| {
            let local = local_tx(tx)?;
            let mut journal = journal_tx(tx)?;
            check_existing(&journal, &proposal.intent, Some(&proposal))?;
            let op = journal
                .operations
                .get_mut(&proposal.intent.operation_id)
                .ok_or_else(denied)?;
            if op
                .decision
                .as_ref()
                .is_some_and(|d| d.kind == DecisionKind::Abort)
                || local.reservation.as_deref() != Some(&proposal.intent.operation_id)
                || local.hold.as_deref() != Some(&proposal.intent.operation_id)
            {
                return Err(conflict("准备期间操作已取消或替换"));
            }
            if let Some(receipt) = &op.prepared {
                return Ok(receipt.clone());
            }
            let receipt = Receipt::sign(&proposal, Domain::Prepare, &identity_tx(tx)?)?;
            op.prepared = Some(receipt.clone());
            write_tx(tx, OPERATIONS_RECORD, &journal)?;
            Ok(receipt)
        })
    }

    pub fn record_prepare(&self, id: String, receipt: PreparedReceipt) -> io::Result<()> {
        self.record_receipt(id, receipt, false)
    }
    pub fn record_installed(&self, id: String, receipt: InstalledReceipt) -> io::Result<()> {
        self.record_receipt(id, receipt, true)
    }
    fn record_receipt(&self, id: String, receipt: Receipt, installed: bool) -> io::Result<()> {
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
            ensure_coordinator(tx, op)?;
            receipt.verify(
                op.proposal.as_ref().ok_or_else(denied)?,
                if installed {
                    Domain::Installed
                } else {
                    Domain::Prepare
                },
            )?;
            if installed
                && !op
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.kind == DecisionKind::Commit)
            {
                return Err(conflict("尚未提交成员操作"));
            }
            let target = if installed {
                &mut op.installations
            } else {
                &mut op.prepares
            };
            if let Some(old) = target.get(&receipt.proof.signer) {
                if old == &receipt {
                    return Ok(());
                }
                return Err(conflict("节点回执不一致"));
            }
            target.insert(receipt.proof.signer.clone(), receipt);
            write_tx(tx, OPERATIONS_RECORD, &journal)
        })
    }

    pub fn decide(&self, id: String, kind: DecisionKind) -> io::Result<Decision> {
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
            ensure_coordinator(tx, op)?;
            if let Some(decision) = &op.decision {
                return if decision.kind == kind {
                    Ok(decision.clone())
                } else {
                    Err(conflict("成员决定已持久化，不能改写"))
                };
            }
            let prepares = if kind == DecisionKind::Commit {
                op.prepares.values().cloned().collect()
            } else {
                vec![]
            };
            let decision = Decision::sign(
                op.intent.clone(),
                op.proposal.clone(),
                kind,
                prepares,
                &identity_tx(tx)?,
            )?;
            op.decision = Some(decision.clone());
            write_tx(tx, OPERATIONS_RECORD, &journal)?;
            Ok(decision)
        })
    }

    pub fn apply_decision(&self, decision: Decision) -> io::Result<Option<InstalledReceipt>> {
        decision.validate()?;
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let mut local = local_tx(tx)?;
            check_existing(&journal, &decision.intent, decision.proposal.as_ref())?;
            let id = decision.intent.operation_id.clone();
            if let Some(op) = journal.operations.get(&id) {
                if let Some(old) = &op.decision {
                    if old != &decision {
                        return Err(conflict("决定与已保存的操作不一致"));
                    }
                }
                if op.installed.is_some() {
                    return Ok(op.installed.clone());
                }
                if op.finished_locally && decision.kind == DecisionKind::Abort {
                    return Ok(None);
                }
            }
            if decision.kind == DecisionKind::Abort {
                if !journal.operations.contains_key(&id) {
                    admit_initial(tx, &local, &decision.intent)?;
                }
                make_room(
                    &mut journal,
                    decision.intent.base.revision,
                    &identity_tx(tx)?.public().member_id,
                )?;
                let op = journal
                    .operations
                    .entry(id.clone())
                    .or_insert_with(|| Operation::new(decision.intent.clone()));
                op.proposal = decision.proposal.clone();
                op.decision = Some(decision);
                op.finished_locally = true;
                release_matching(&mut local, &id, false);
                write_tx(tx, LOCAL_RECORD, &local)?;
                write_tx(tx, OPERATIONS_RECORD, &journal)?;
                return Ok(None);
            }
            let proposal = decision.proposal.as_ref().ok_or_else(denied)?;
            let op = journal
                .operations
                .get_mut(&id)
                .ok_or_else(|| conflict("提交前必须先持久化准备"))?;
            if op.prepared.is_none()
                || local.reservation.as_deref() != Some(&id)
                || local.hold.as_deref() != Some(&id)
            {
                return Err(conflict("提交前必须先完成停机准备"));
            }
            let member_id = identity_tx(tx)?.public().member_id.clone();
            let current: Option<Manifest> = read_tx(tx, MANIFEST_RECORD)?;
            // A node that left by recognition still holds the manifest from
            // before it was removed. That copy is not authority: the current
            // key is absent and the node is only joining again.
            let stale_departure =
                matches!(local.lifecycle, Lifecycle::JoinReady | Lifecycle::Pairing)
                    && current
                        .as_ref()
                        .is_some_and(|manifest| manifest.member(&member_id).is_none());
            if !stale_departure
                && current
                    .as_ref()
                    .is_some_and(|m| m != &proposal.intent.base && m != &proposal.desired)
            {
                return Err(conflict("拒绝过期成员提交"));
            }
            let retained = proposal.desired.member(&member_id).is_some();
            project_config(
                tx,
                if retained {
                    Some(&proposal.desired)
                } else {
                    None
                },
                &member_id,
                !retained,
            )?;
            write_tx(tx, MANIFEST_RECORD, &proposal.desired)?;
            if retained {
                local.lifecycle = Lifecycle::Managed;
                local.descriptor = proposal.desired.member(&member_id).cloned();
            }
            // Departing members retain the matching hold through FINISH. Their
            // monitor choices have already been persisted off above.
            local.pairing = None;
            let receipt = Receipt::sign(proposal, Domain::Installed, &identity_tx(tx)?)?;
            op.decision = Some(decision);
            op.installed = Some(receipt.clone());
            write_tx(tx, LOCAL_RECORD, &local)?;
            write_tx(tx, OPERATIONS_RECORD, &journal)?;
            Ok(Some(receipt))
        })
    }

    pub fn decide_finish(&self, id: String) -> io::Result<Finish> {
        self.decide_finish_with_safety(id, None)
    }
    pub fn decide_finish_with_safety(
        &self,
        id: String,
        safety: Option<RemovalSafety>,
    ) -> io::Result<Finish> {
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
            ensure_coordinator(tx, op)?;
            if let Some(finish) = &op.finish {
                return Ok(finish.clone());
            }
            let decision = op.decision.clone().ok_or_else(denied)?;
            let installed = op.installations.values().cloned().collect();
            let identity = identity_tx(tx)?;
            let finish = if safety.is_some() {
                Finish::sign_with_safety(decision, installed, safety, &identity)?
            } else {
                Finish::sign(decision, installed, &identity)?
            };
            op.finish = Some(finish.clone());
            write_tx(tx, OPERATIONS_RECORD, &journal)?;
            Ok(finish)
        })
    }
    pub fn apply_finish(&self, finish: Finish) -> io::Result<()> {
        finish.validate()?;
        finish.validate_removal_safety()?;
        self.store.transaction(move |tx| {
            let mut local = local_tx(tx)?;
            let mut journal = journal_tx(tx)?;
            let id = &finish.decision.intent.operation_id;
            check_existing(
                &journal,
                &finish.decision.intent,
                finish.decision.proposal.as_ref(),
            )?;
            let op = journal.operations.get_mut(id).ok_or_else(denied)?;
            if op.decision.as_ref() != Some(&finish.decision) || op.installed.is_none() {
                return Err(conflict("本节点尚未安装成员决定"));
            }
            if op.finish.as_ref().is_some_and(|old| old != &finish) {
                return Err(conflict("完成证明不一致"));
            }
            if op.finished_locally {
                return Ok(());
            }
            let retained = finish
                .decision
                .proposal
                .as_ref()
                .ok_or_else(denied)?
                .desired
                .member(&identity_tx(tx)?.public().member_id)
                .is_some();
            release_matching(&mut local, id, !retained);
            if retained {
                if let Change::Remove {
                    target_member_id, ..
                } = &finish.decision.intent.change
                {
                    let source = finish
                        .decision
                        .intent
                        .base
                        .member(target_member_id)
                        .ok_or_else(denied)?;
                    local
                        .removed_sources
                        .insert(source.node_id.clone(), finish.clone());
                    // Missing old evidence only blocks a stale runtime handoff;
                    // never interpret its absence as permission to take over.
                    if local.removed_sources.len() > MAX_MEMBERS {
                        if let Some(oldest) = local
                            .removed_sources
                            .iter()
                            .min_by_key(|(_, proof)| proof.decision.intent.base.revision)
                            .map(|(id, _)| id.clone())
                        {
                            local.removed_sources.remove(&oldest);
                        }
                    }
                }
            }
            op.finish = Some(finish);
            op.finished_locally = true;
            write_tx(tx, LOCAL_RECORD, &local)?;
            write_tx(tx, OPERATIONS_RECORD, &journal)
        })
    }
    pub fn delivered(&self, id: String, member: String) -> io::Result<()> {
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
            ensure_coordinator(tx, op)?;
            op.delivered.insert(member);
            write_tx(tx, OPERATIONS_RECORD, &journal)
        })
    }
    pub fn note(&self, id: String, message: String) -> io::Result<()> {
        self.store.transaction(move |tx| {
            let mut journal = journal_tx(tx)?;
            let op = journal.operations.get_mut(&id).ok_or_else(denied)?;
            op.message = Some(message);
            write_tx(tx, OPERATIONS_RECORD, &journal)
        })
    }

    /// Preconditions for a local leave. Network probes happen only after this
    /// returns, and never while a hold or unfinished operation is recorded.
    pub fn departure_plan(&self) -> io::Result<DeparturePlan> {
        let local = self.local()?;
        if local.lifecycle != Lifecycle::Managed {
            return Err(conflict("只有已加入集群的服务器可以这样退出"));
        }
        if local.hold.is_some() || local.reservation.is_some() || local.pairing.is_some() {
            return Err(conflict("本节点正在维护，不能退出集群"));
        }
        let journal = read::<Journal>(&self.store, OPERATIONS_RECORD)?.unwrap_or_default();
        if unfinished(&journal) {
            return Err(conflict("上一项成员操作尚未完成，不能退出集群"));
        }
        let manifest = self.manifest()?.ok_or_else(denied)?;
        manifest.validate()?;
        let identity = self.identity()?;
        let me = local.descriptor.as_ref().ok_or_else(denied)?;
        if me.member_id != identity.public().member_id || manifest.member(&me.member_id).is_none() {
            return Err(conflict("本地成员状态已改变，请刷新"));
        }
        let others = manifest
            .members
            .iter()
            .filter(|member| member.member_id != me.member_id)
            .cloned()
            .collect::<Vec<_>>();
        if others.is_empty() {
            return Err(conflict("集群中没有其他服务器可以确认，请使用「移除」"));
        }
        Ok(DeparturePlan {
            cluster_id: manifest.cluster_id,
            revision: manifest.revision,
            self_key: identity.public().clone(),
            others,
        })
    }

    /// Re-checks managed, unheld and still listed, then applies the same
    /// monitors-off projection and left hold a reached node records when it
    /// finishes departure. The write commits only after those checks.
    pub fn commit_local_departure(&self, proved: BTreeSet<String>) -> io::Result<()> {
        self.store.transaction(move |tx| {
            let mut local = local_tx(tx)?;
            if local.lifecycle != Lifecycle::Managed
                || local.hold.is_some()
                || local.reservation.is_some()
                || local.pairing.is_some()
            {
                return Err(conflict("本节点正在维护或尚未加入，不能退出"));
            }
            if unfinished(&journal_tx(tx)?) {
                return Err(conflict("上一项成员操作尚未完成，不能退出集群"));
            }
            let identity = identity_tx(tx)?;
            let manifest: Manifest = read_tx(tx, MANIFEST_RECORD)?.ok_or_else(denied)?;
            manifest.validate()?;
            let me = local.descriptor.as_ref().ok_or_else(denied)?;
            if me.member_id != identity.public().member_id
                || manifest.member(&me.member_id).is_none()
            {
                return Err(conflict("本地成员状态已改变，请刷新"));
            }
            let others = manifest
                .members
                .iter()
                .filter(|member| member.member_id != me.member_id)
                .map(|member| member.member_id.clone())
                .collect::<BTreeSet<_>>();
            if others.is_empty() {
                return Err(conflict("集群中没有其他服务器可以确认，请使用「移除」"));
            }
            if others != proved {
                return Err(conflict("成员清单已改变，请刷新后重试"));
            }
            let member_id = me.member_id.clone();
            project_config(tx, None, &member_id, true)?;
            local.lifecycle = Lifecycle::Left;
            local.hold = Some("left".into());
            local.pairing = None;
            local.reservation = None;
            write_tx(tx, LOCAL_RECORD, &local)
        })
    }
}

#[derive(Clone, Debug)]
pub struct DeparturePlan {
    pub cluster_id: String,
    pub revision: u64,
    pub self_key: PublicIdentity,
    pub others: Vec<Descriptor>,
}

fn unfinished(journal: &Journal) -> bool {
    journal
        .operations
        .values()
        .any(|op| !op.terminal() || !op.finished_locally)
}

/// Read-only. `Absent` is returned only when this committed manifest does not
/// contain that exact key. A different cluster, or no manifest, is not absence.
pub fn recognition_verdict(
    store: &Store,
    probe: &RecognitionProbe,
) -> io::Result<RecognitionVerdict> {
    let public = probe.identity();
    public.validate()?;
    let Some(manifest) = read::<Manifest>(store, MANIFEST_RECORD)? else {
        return Err(denied());
    };
    manifest.validate()?;
    if manifest.cluster_id != probe.cluster_id {
        return Err(denied());
    }
    let present = manifest
        .member(&public.member_id)
        .is_some_and(|member| member.public_key == public.public_key);
    Ok(if present {
        RecognitionVerdict::Present
    } else {
        RecognitionVerdict::Absent
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupRequest {
    pub operation_id: String,
    pub expected_local_revision: u64,
    pub node_id: String,
    pub name: String,
    pub api_url: String,
    pub priority: i32,
}

pub fn execution_hold(store: &Store) -> io::Result<Option<String>> {
    let docs = store.read_many(&[LOCAL_RECORD, MANIFEST_RECORD, "config.json"])?;
    if let Some(doc) = docs.get(LOCAL_RECORD) {
        let local: LocalState = decode(doc.value.clone())?;
        if local.version != PROTOCOL {
            return Err(invalid("本地成员状态不兼容"));
        }
        if local.hold.is_some()
            || !matches!(local.lifecycle, Lifecycle::Managed | Lifecycle::Standalone)
        {
            return Ok(Some("成员维护或未加入状态，暂停本节点执行".into()));
        }
    } else if docs.contains_key(MANIFEST_RECORD)
        || docs
            .get("config.json")
            .is_some_and(|d| d.value["cluster"]["enabled"] == true)
    {
        return Ok(Some("旧集群尚未配置新成员身份，暂停本节点执行".into()));
    }
    Ok(None)
}

/// Consistent committed authority. Operation admission is deliberately separate
/// and never lets pairing or recovery rights become ordinary peer privileges.
pub fn trust_snapshot(store: &Store) -> io::Result<TrustSnapshot> {
    let docs = store.read_many(&[LOCAL_RECORD, MANIFEST_RECORD])?;
    let local: LocalState = decode(docs.get(LOCAL_RECORD).ok_or_else(denied)?.value.clone())?;
    let manifest: Manifest = decode(docs.get(MANIFEST_RECORD).ok_or_else(denied)?.value.clone())?;
    manifest.validate()?;
    if local.lifecycle != Lifecycle::Managed
        || local
            .descriptor
            .as_ref()
            .is_none_or(|d| manifest.member(&d.member_id).is_none())
    {
        return Err(denied());
    }
    Ok(TrustSnapshot {
        cluster_id: manifest.cluster_id,
        revisions: vec![manifest.revision],
        scope: PeerScope::Ordinary,
        members: manifest.members.into_iter().map(|m| m.identity()).collect(),
    })
}

pub fn operation_trust(store: &Store, intent: &Intent) -> io::Result<TrustSnapshot> {
    intent.validate()?;
    let docs = store.read_many(&[LOCAL_RECORD, MANIFEST_RECORD, OPERATIONS_RECORD])?;
    let local: LocalState = docs
        .get(LOCAL_RECORD)
        .map(|d| decode(d.value.clone()))
        .transpose()?
        .unwrap_or_default();
    let journal: Journal = docs
        .get(OPERATIONS_RECORD)
        .map(|d| decode(d.value.clone()))
        .transpose()?
        .unwrap_or_default();
    let current: Option<Manifest> = docs
        .get(MANIFEST_RECORD)
        .map(|d| decode(d.value.clone()))
        .transpose()?;
    let existing = journal.operations.get(&intent.operation_id);
    let mut members: Vec<PublicIdentity> = vec![];
    let mut revisions = vec![intent.base.revision];
    if let Some(op) = existing {
        if op.intent != *intent {
            return Err(denied());
        }
        if let Some(proposal) = &op.proposal {
            revisions.push(proposal.desired.revision);
            // Revoked identities have no operation authority on a retained node
            // after installation. The retained coordinator survives revocation.
            let installed = current
                .as_ref()
                .is_some_and(|m| m.revision >= proposal.desired.revision);
            members = if installed {
                proposal.desired.members.iter()
            } else {
                proposal.destinations.iter()
            }
            .map(Descriptor::identity)
            .collect();
        } else {
            members = intent
                .base
                .members
                .iter()
                .map(Descriptor::identity)
                .collect();
        }
    } else if local.pairing.as_ref().is_some_and(|p| p.intent == *intent) {
        members.push(
            intent
                .base
                .member(&intent.coordinator)
                .ok_or_else(denied)?
                .identity(),
        );
    } else if local.lifecycle == Lifecycle::Managed && current.as_ref() == Some(&intent.base) {
        // Initial PREPARE / ABORT only. Router must restrict this scope to those
        // bootstrap verbs; it is not authority to query unrelated operations.
        members.push(
            intent
                .base
                .member(&intent.coordinator)
                .ok_or_else(denied)?
                .identity(),
        );
    } else {
        return Err(denied());
    }
    if let Some(current) = current {
        if current.revision > intent.base.revision {
            members.retain(|m| current.member(&m.member_id).is_some());
        }
    }
    Ok(TrustSnapshot {
        cluster_id: intent.base.cluster_id.clone(),
        revisions,
        scope: PeerScope::Operation(intent.operation_id.clone()),
        members,
    })
}

fn join_error(_: tokio::task::JoinError) -> io::Error {
    io::Error::other("成员事务工作线程失败")
}
fn read<T: serde::de::DeserializeOwned>(store: &Store, name: &str) -> io::Result<Option<T>> {
    store.read(name)?.map(|d| decode(d.value)).transpose()
}
fn read_tx<T: serde::de::DeserializeOwned>(
    tx: &Transaction<'_>,
    name: &str,
) -> io::Result<Option<T>> {
    tx.read(name)?.map(|d| decode(d.value)).transpose()
}
fn write_tx<T: Serialize>(tx: &mut Transaction<'_>, name: &str, value: &T) -> io::Result<()> {
    tx.write(
        name,
        serde_json::to_value(value).map_err(|_| invalid("成员记录编码失败"))?,
    )?;
    Ok(())
}
fn local_tx(tx: &Transaction<'_>) -> io::Result<LocalState> {
    Ok(read_tx(tx, LOCAL_RECORD)?.unwrap_or_default())
}
fn journal_tx(tx: &Transaction<'_>) -> io::Result<Journal> {
    Ok(read_tx(tx, OPERATIONS_RECORD)?.unwrap_or_default())
}
fn identity_tx(tx: &Transaction<'_>) -> io::Result<NodeIdentity> {
    NodeIdentity::load_from(tx)?.ok_or_else(denied)
}
fn ensure_coordinator(tx: &Transaction<'_>, op: &Operation) -> io::Result<()> {
    if identity_tx(tx)?.public().member_id != op.intent.coordinator {
        return Err(denied());
    }
    Ok(())
}
fn check_existing(
    journal: &Journal,
    intent: &Intent,
    proposal: Option<&Proposal>,
) -> io::Result<()> {
    if let Some(op) = journal.operations.get(&intent.operation_id) {
        if op.intent != *intent
            || (op.proposal.is_some() && proposal.is_some() && op.proposal.as_ref() != proposal)
        {
            return Err(conflict("同一操作标识不能更改内容"));
        }
    }
    Ok(())
}
fn admit_initial(tx: &Transaction<'_>, local: &LocalState, intent: &Intent) -> io::Result<()> {
    if local.pairing.as_ref().is_some_and(|p| p.intent == *intent) {
        return Ok(());
    }
    let current: Option<Manifest> = read_tx(tx, MANIFEST_RECORD)?;
    if local.lifecycle != Lifecycle::Managed || current.as_ref() != Some(&intent.base) {
        return Err(conflict("成员基础版本已改变"));
    }
    Ok(())
}
fn make_room(journal: &mut Journal, base: u64, local_id: &str) -> io::Result<()> {
    if journal.operations.len() < MAX_OPERATIONS {
        return Ok(());
    }
    // Tombstones for the current base must remain: delayed PREPARE is still
    // admissible. Older terminal records cannot vote against a newer manifest.
    journal.operations.retain(|_, o| {
        !o.terminal()
            || !o.finished_locally
            || o.intent.base.revision >= base
            || (o.intent.coordinator == local_id && o.cleanup_pending())
    });
    if journal.operations.len() >= MAX_OPERATIONS {
        return Err(conflict("成员操作记录已满，请先恢复待处理操作"));
    }
    Ok(())
}
fn release_matching(local: &mut LocalState, id: &str, departed: bool) {
    if local.reservation.as_deref() == Some(id) {
        local.reservation = None;
    }
    if local.hold.as_deref() != Some(id) {
        return;
    }
    if departed {
        local.lifecycle = Lifecycle::Left;
        local.hold = Some("left".into());
        local.pairing = None;
    } else if matches!(local.lifecycle, Lifecycle::Pairing | Lifecycle::JoinReady) {
        local.lifecycle = Lifecycle::JoinReady;
        local.hold = Some("join_ready".into());
        local.pairing = None;
    } else {
        local.hold = None;
    }
}

fn project_config(
    tx: &mut Transaction<'_>,
    manifest: Option<&Manifest>,
    local_id: &str,
    turn_off: bool,
) -> io::Result<()> {
    let Some(document) = tx.read("config.json")? else {
        return Err(invalid("请先完成服务器初始设置"));
    };
    let mut config = document.value;
    if !config["cluster"].is_object() {
        config["cluster"] = serde_json::to_value(crate::config::ClusterConfig::default())
            .map_err(|_| invalid("集群设置无效"))?;
    }
    let cluster = config["cluster"]
        .as_object_mut()
        .ok_or_else(|| invalid("集群设置无效"))?;
    if let Some(manifest) = manifest {
        let local = manifest.member(local_id).ok_or_else(denied)?;
        cluster.insert("enabled".into(), json!(true));
        cluster.insert("node_id".into(), json!(local.node_id));
        cluster.insert("node_name".into(), json!(local.name));
        cluster.insert("public_api_url".into(), json!(local.api_url));
        cluster.insert("priority".into(), json!(local.priority));
        cluster.insert("peers".into(),Value::Array(manifest.members.iter().filter(|m|m.member_id!=local_id).map(|m|json!({"node_id":m.node_id,"name":m.name,"api_url":m.api_url,"priority":m.priority})).collect()));
        if !cluster.get("public_status").is_some_and(Value::is_object) {
            cluster.insert("public_status".into(), json!({}));
        }
        cluster.get_mut("public_status").ok_or_else(denied)?["node_id"] = json!(manifest
            .public_member_id
            .as_ref()
            .and_then(|id| manifest.member(id))
            .map(|m| m.node_id.as_str())
            .unwrap_or(""));
    } else {
        cluster.insert("enabled".into(), json!(false));
        cluster.insert("peers".into(), json!([]));
        if let Some(public) = cluster.get_mut("public_status") {
            public["node_id"] = json!("");
        }
    }
    if turn_off {
        config["enable_youtube_monitor"] = json!(false);
        config["enable_twitch_monitor"] = json!(false);
        for path in [
            ["youtube", "enable_monitor"],
            ["twitch", "enable_monitor"],
            ["niconico", "enable_monitor"],
            ["priority_channel", "enabled"],
            ["priority_channel", "auto_restart"],
            ["bililive", "enable_danmaku_command"],
        ] {
            if config[path[0]].is_object() {
                config[path[0]][path[1]] = json!(false);
            }
        }
    }
    tx.write("config.json", config)?;
    Ok(())
}
