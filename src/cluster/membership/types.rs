use super::{denied, encode, invalid};
use crate::cluster::peer_auth::{
    digest_hex, verify_proof, Domain, NodeIdentity, PeerEndpoint, PublicIdentity, SignedProof,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io};

pub const PROTOCOL: u32 = 1;
pub const MAX_MEMBERS: usize = 64;

/// Members advertise verified HTTPS. A literal loopback HTTP address is the
/// operator's explicit choice of an already authenticated local tunnel.
pub fn member_endpoint(url: &str) -> io::Result<PeerEndpoint> {
    PeerEndpoint::parse(url, true)
}

pub fn validate_operation_id(value: &str) -> io::Result<()> {
    let compact: String = value.chars().filter(|c| *c != '-').collect();
    if (value.len() != 32 && value.len() != 36)
        || compact.len() != 32
        || !compact
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || (value.len() == 36 && [8, 13, 18, 23].iter().any(|i| value.as_bytes()[*i] != b'-'))
    {
        return Err(invalid("操作标识格式无效"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub member_id: String,
    pub public_key: String,
    pub node_id: String,
    pub name: String,
    pub api_url: String,
    pub priority: i32,
}
impl Descriptor {
    pub fn identity(&self) -> PublicIdentity {
        PublicIdentity {
            member_id: self.member_id.clone(),
            public_key: self.public_key.clone(),
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        self.identity().validate()?;
        if self.node_id.is_empty()
            || self.node_id.len() > 64
            || !self
                .node_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err(invalid("节点标识或名称无效"));
        }
        member_endpoint(&self.api_url)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub cluster_id: String,
    pub revision: u64,
    pub digest: String,
    pub members: Vec<Descriptor>,
    pub public_member_id: Option<String>,
}
impl Manifest {
    pub fn new(
        cluster_id: String,
        revision: u64,
        mut members: Vec<Descriptor>,
        public_member_id: Option<String>,
    ) -> io::Result<Self> {
        members.sort_by(|a, b| a.member_id.cmp(&b.member_id));
        let mut result = Self {
            version: PROTOCOL,
            cluster_id,
            revision,
            digest: String::new(),
            members,
            public_member_id,
        };
        result.digest = result.computed_digest()?;
        result.validate()?;
        Ok(result)
    }
    pub fn member(&self, id: &str) -> Option<&Descriptor> {
        self.members.iter().find(|m| m.member_id == id)
    }
    fn computed_digest(&self) -> io::Result<String> {
        Ok(digest_hex(&encode(&(
            self.version,
            &self.cluster_id,
            self.revision,
            &self.members,
            &self.public_member_id,
        ))?))
    }
    pub fn validate(&self) -> io::Result<()> {
        validate_operation_id(&self.cluster_id)?;
        if self.version != PROTOCOL
            || self.revision == 0
            || self.members.is_empty()
            || self.members.len() > MAX_MEMBERS
            || self.digest != self.computed_digest()?
        {
            return Err(invalid("成员清单无效"));
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut urls = BTreeSet::new();
        let mut previous = "";
        for member in &self.members {
            member.validate()?;
            if member.member_id.as_str() <= previous
                || !ids.insert(&member.member_id)
                || !names.insert(&member.node_id)
                || !urls.insert(
                    member_endpoint(&member.api_url)?
                        .as_str()
                        .trim_end_matches('/')
                        .to_owned(),
                )
            {
                return Err(invalid("成员身份、节点标识或地址重复"));
            }
            previous = &member.member_id;
        }
        if self
            .public_member_id
            .as_ref()
            .is_some_and(|id| self.member(id).is_none())
        {
            return Err(invalid("状态页节点必须属于成员清单"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Add {
        target_url: String,
    },
    Remove {
        target_member_id: String,
        replacement_public_member_id: Option<String>,
    },
    UpdateNode {
        target_member_id: String,
        name: String,
        api_url: String,
        priority: i32,
    },
    SetPublicNode {
        public_member_id: Option<String>,
    },
}
impl Change {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::Remove { .. } => "remove",
            Self::UpdateNode { .. } => "update_node",
            Self::SetPublicNode { .. } => "set_public_node",
        }
    }
}

/// Persisted before the first password-bearing call. The password is never a
/// field of this type, a proposal, a Debug implementation or a journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub version: u32,
    pub operation_id: String,
    pub sequence: u64,
    pub coordinator: String,
    pub initiator: String,
    pub base: Manifest,
    pub change: Change,
}
impl Intent {
    pub fn digest(&self) -> io::Result<String> {
        Ok(digest_hex(&encode(self)?))
    }
    pub fn validate(&self) -> io::Result<()> {
        validate_operation_id(&self.operation_id)?;
        self.base.validate()?;
        if self.version != PROTOCOL
            || self.sequence == 0
            || self.base.member(&self.coordinator).is_none()
            || self.base.member(&self.initiator).is_none()
        {
            return Err(invalid("成员操作发起者无效"));
        }
        match &self.change {
            Change::Add { target_url } => {
                member_endpoint(target_url)?;
            }
            Change::Remove {
                target_member_id, ..
            } => {
                if self.base.member(target_member_id).is_none()
                    || target_member_id == &self.coordinator
                {
                    return Err(invalid("协调节点必须保留在成员中"));
                }
            }
            Change::UpdateNode {
                target_member_id,
                api_url,
                ..
            } => {
                if self.base.member(target_member_id).is_none() {
                    return Err(invalid("节点不属于集群"));
                }
                member_endpoint(api_url)?;
            }
            Change::SetPublicNode { public_member_id } => {
                if public_member_id
                    .as_ref()
                    .is_some_and(|id| self.base.member(id).is_none())
                {
                    return Err(invalid("状态页节点不属于集群"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingReceipt {
    pub intent_digest: String,
    pub candidate: Descriptor,
    pub proof: SignedProof,
}
impl PairingReceipt {
    pub fn sign(
        intent: &Intent,
        candidate: Descriptor,
        identity: &NodeIdentity,
    ) -> io::Result<Self> {
        let intent_digest = intent.digest()?;
        let proof = identity.sign(Domain::Pairing, &encode(&(&intent_digest, &candidate))?);
        Ok(Self {
            intent_digest,
            candidate,
            proof,
        })
    }
    pub fn verify(&self, intent: &Intent) -> io::Result<()> {
        self.candidate.validate()?;
        let Change::Add { target_url } = &intent.change else {
            return Err(denied());
        };
        if self.intent_digest != intent.digest()?
            || member_endpoint(target_url)?.as_str().trim_end_matches('/')
                != member_endpoint(&self.candidate.api_url)?
                    .as_str()
                    .trim_end_matches('/')
        {
            return Err(denied());
        }
        verify_proof(
            &self.candidate.identity(),
            Domain::Pairing,
            &encode(&(&self.intent_digest, &self.candidate))?,
            &self.proof,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub intent: Intent,
    pub desired: Manifest,
    pub pairing: Option<PairingReceipt>,
    /// Every possible recipient, including a departing node. This is immutable.
    pub destinations: Vec<Descriptor>,
    pub proof: SignedProof,
}
impl Proposal {
    pub fn build(
        intent: Intent,
        pairing: Option<PairingReceipt>,
        identity: &NodeIdentity,
    ) -> io::Result<Self> {
        intent.validate()?;
        let desired = desired_manifest(&intent, pairing.as_ref())?;
        let mut destinations = intent.base.members.clone();
        for member in &desired.members {
            if !destinations.iter().any(|m| m.member_id == member.member_id) {
                destinations.push(member.clone());
            }
        }
        destinations.sort_by(|a, b| a.member_id.cmp(&b.member_id));
        let proof = identity.sign(
            Domain::Proposal,
            &encode(&(&intent, &desired, &pairing, &destinations))?,
        );
        let result = Self {
            intent,
            desired,
            pairing,
            destinations,
            proof,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn digest(&self) -> io::Result<String> {
        Ok(digest_hex(&encode(&(
            &self.intent,
            &self.desired,
            &self.pairing,
            &self.destinations,
        ))?))
    }
    pub fn validate(&self) -> io::Result<()> {
        self.intent.validate()?;
        self.desired.validate()?;
        if self.desired != desired_manifest(&self.intent, self.pairing.as_ref())? {
            return Err(denied());
        }
        let mut destinations = self.intent.base.members.clone();
        for m in &self.desired.members {
            if !destinations.iter().any(|old| old.member_id == m.member_id) {
                destinations.push(m.clone());
            }
        }
        destinations.sort_by(|a, b| a.member_id.cmp(&b.member_id));
        if destinations != self.destinations {
            return Err(denied());
        }
        let coordinator = self
            .intent
            .base
            .member(&self.intent.coordinator)
            .ok_or_else(denied)?;
        verify_proof(
            &coordinator.identity(),
            Domain::Proposal,
            &encode(&(
                &self.intent,
                &self.desired,
                &self.pairing,
                &self.destinations,
            ))?,
            &self.proof,
        )
    }
    pub fn member(&self, id: &str) -> Option<&Descriptor> {
        self.destinations.iter().find(|m| m.member_id == id)
    }
}

fn desired_manifest(intent: &Intent, pairing: Option<&PairingReceipt>) -> io::Result<Manifest> {
    let mut members = intent.base.members.clone();
    let mut public = intent.base.public_member_id.clone();
    match &intent.change {
        Change::Add { .. } => {
            let receipt = pairing.ok_or_else(|| invalid("缺少目标配对确认"))?;
            receipt.verify(intent)?;
            members.push(receipt.candidate.clone());
        }
        Change::Remove {
            target_member_id,
            replacement_public_member_id,
        } => {
            members.retain(|m| &m.member_id != target_member_id);
            if public.as_ref() == Some(target_member_id) {
                public = replacement_public_member_id.clone();
            } else if replacement_public_member_id.is_some() {
                return Err(invalid("只有移除状态页节点时才能指定替代节点"));
            }
        }
        Change::UpdateNode {
            target_member_id,
            name,
            api_url,
            priority,
        } => {
            let member = members
                .iter_mut()
                .find(|m| &m.member_id == target_member_id)
                .ok_or_else(denied)?;
            member.name = name.clone();
            member.api_url = api_url.clone();
            member.priority = *priority;
        }
        Change::SetPublicNode { public_member_id } => public = public_member_id.clone(),
    }
    if !matches!(intent.change, Change::Add { .. }) && pairing.is_some() {
        return Err(denied());
    }
    Manifest::new(
        intent.base.cluster_id.clone(),
        intent
            .base
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("成员版本已达上限"))?,
        members,
        public,
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub proposal_digest: String,
    pub proof: SignedProof,
}
impl Receipt {
    pub fn sign(proposal: &Proposal, domain: Domain, identity: &NodeIdentity) -> io::Result<Self> {
        let proposal_digest = proposal.digest()?;
        let proof = identity.sign(domain, proposal_digest.as_bytes());
        Ok(Self {
            proposal_digest,
            proof,
        })
    }
    pub fn verify(&self, proposal: &Proposal, domain: Domain) -> io::Result<()> {
        if self.proposal_digest != proposal.digest()? {
            return Err(denied());
        }
        let member = proposal.member(&self.proof.signer).ok_or_else(denied)?;
        verify_proof(
            &member.identity(),
            domain,
            self.proposal_digest.as_bytes(),
            &self.proof,
        )
    }
}
pub type PreparedReceipt = Receipt;
pub type InstalledReceipt = Receipt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Commit,
    Abort,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub intent: Intent,
    pub proposal: Option<Proposal>,
    pub kind: DecisionKind,
    pub prepares: Vec<PreparedReceipt>,
    pub proof: SignedProof,
}
impl Decision {
    pub fn sign(
        intent: Intent,
        proposal: Option<Proposal>,
        kind: DecisionKind,
        mut prepares: Vec<PreparedReceipt>,
        identity: &NodeIdentity,
    ) -> io::Result<Self> {
        prepares.sort_by(|a, b| a.proof.signer.cmp(&b.proof.signer));
        let proof = identity.sign(
            Domain::Decision,
            &encode(&(&intent, &proposal, kind, &prepares))?,
        );
        let result = Self {
            intent,
            proposal,
            kind,
            prepares,
            proof,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> io::Result<()> {
        self.intent.validate()?;
        if let Some(p) = &self.proposal {
            p.validate()?;
            if p.intent != self.intent {
                return Err(denied());
            }
        }
        if self.kind == DecisionKind::Commit {
            let p = self.proposal.as_ref().ok_or_else(denied)?;
            let signers = verify_receipts(p, &self.prepares, Domain::Prepare)?;
            let old_count = p
                .intent
                .base
                .members
                .iter()
                .filter(|m| signers.contains(m.member_id.as_str()))
                .count();
            if old_count < p.intent.base.members.len() / 2 + 1
                || p.desired
                    .members
                    .iter()
                    .any(|m| !signers.contains(m.member_id.as_str()))
            {
                return Err(invalid("必须取得原成员多数及全部保留成员的停机确认"));
            }
        } else if !self.prepares.is_empty() {
            return Err(denied());
        }
        verify_proof(
            &self
                .intent
                .base
                .member(&self.intent.coordinator)
                .ok_or_else(denied)?
                .identity(),
            Domain::Decision,
            &encode(&(&self.intent, &self.proposal, self.kind, &self.prepares))?,
            &self.proof,
        )
    }
}

fn verify_receipts<'a>(
    proposal: &Proposal,
    receipts: &'a [Receipt],
    domain: Domain,
) -> io::Result<BTreeSet<&'a str>> {
    let mut signers = BTreeSet::new();
    let mut previous = "";
    for receipt in receipts {
        receipt.verify(proposal, domain)?;
        if receipt.proof.signer.as_str() <= previous
            || !signers.insert(receipt.proof.signer.as_str())
        {
            return Err(denied());
        }
        previous = &receipt.proof.signer;
    }
    Ok(signers)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finish {
    pub decision: Decision,
    pub installed: Vec<InstalledReceipt>,
    pub proof: SignedProof,
}
impl Finish {
    pub fn sign(
        decision: Decision,
        mut installed: Vec<InstalledReceipt>,
        identity: &NodeIdentity,
    ) -> io::Result<Self> {
        installed.sort_by(|a, b| a.proof.signer.cmp(&b.proof.signer));
        let proof = identity.sign(Domain::Finish, &encode(&(&decision, &installed))?);
        let result = Self {
            decision,
            installed,
            proof,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> io::Result<()> {
        self.decision.validate()?;
        if self.decision.kind != DecisionKind::Commit {
            return Err(denied());
        }
        let p = self.decision.proposal.as_ref().ok_or_else(denied)?;
        let signers = verify_receipts(p, &self.installed, Domain::Installed)?;
        if p.desired
            .members
            .iter()
            .any(|m| !signers.contains(m.member_id.as_str()))
        {
            return Err(invalid("全部保留成员安装完成前不能结束维护"));
        }
        verify_proof(
            &p.intent
                .base
                .member(&p.intent.coordinator)
                .ok_or_else(denied)?
                .identity(),
            Domain::Finish,
            &encode(&(&self.decision, &self.installed))?,
            &self.proof,
        )
    }
}
