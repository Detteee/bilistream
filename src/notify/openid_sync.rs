//! Batch 1b: the admin openid between current members (contract
//! "Openid at rest and across nodes" and "Single writer").
//!
//! - The single writer pushes its sealed `notify-openid` record to every
//!   current member after a bind, a change or an unbind, retrying a member
//!   until it acks (`push_all`).
//! - A node that joins, restarts into a cluster or becomes the writer pulls
//!   once from every member (`pull_all`); an unbound member answers with an
//!   explicit `unbound` state and its revision, so stale copies lose.
//! - Receivers keep the greatest `(openid_revision, writer_node_id)`
//!   (`store::accept_copy`), so concurrent writes on two partition sides
//!   converge on one value.
//! - Everything runs within ONE membership revision: the member list and
//!   every call are checked against it, and the receiving route accepts
//!   only committed members of its own current revision. A removed node
//!   has no trust snapshot, so it can neither push nor pull, and it wiped
//!   its copy in the membership transaction that removed it.
//!
//! The value travels only on the signed peer routes below (HTTPS or the
//! authenticated loopback tunnel); logs say `openid=<set>`.

use super::store::{self, CopyOutcome, OpenidCopy};
use crate::storage::Store;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// A peer call that did not land. No text: nothing from the peer is echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    Unreachable,
    Refused,
    /// The membership revision moved since the member list was read.
    MembershipChanged,
}

/// What a receiver answers to a push (never the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushAck {
    pub applied: bool,
    pub revision: u64,
}

impl From<CopyOutcome> for PushAck {
    fn from(outcome: CopyOutcome) -> Self {
        Self {
            applied: outcome.applied,
            revision: outcome.revision,
        }
    }
}

/// The member set of one membership revision and the calls into it.
// async_trait marks the boxed futures `#[must_use]` twice.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait PeerLink: Send + Sync {
    /// `(membership_revision, other member node ids)`. Fails when this node
    /// is not a current member or the membership is changing.
    fn members(&self) -> io::Result<(u64, Vec<String>)>;
    async fn push(
        &self,
        revision: u64,
        node: &str,
        copy: &OpenidCopy,
    ) -> Result<PushAck, LinkError>;
    async fn pull(&self, revision: u64, node: &str) -> Result<OpenidCopy, LinkError>;
    /// Whether this node is in (or was removed from) a managed cluster.
    /// `false` on a single-node install: there is nobody to push to, so a
    /// bind or unbind skips the push quietly (R4-1).
    fn clustered(&self) -> bool {
        true
    }
    /// Addendum F rule 7: the peer's heartbeat advertises
    /// `credentials_sync`. Others get the copy without the set.
    fn credentials_sync(&self, _node: &str) -> bool {
        true
    }
    /// Addendum F rule 2: forward a credential save to the gateway holder.
    async fn forward(
        &self,
        _revision: u64,
        _node: &str,
        _save: &super::credentials::CredentialSave,
    ) -> Result<super::credentials::SaveOutcome, super::credentials::SaveError> {
        Err(super::credentials::SaveError::HolderUnreachable)
    }
}

/// Unbind is "retried until acked"; a member still offline after the last
/// attempt clears on its next pull.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub first_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub const PRODUCTION: RetryPolicy = RetryPolicy {
        attempts: 12,
        first_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(60),
    };

    fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u32 << attempt.saturating_sub(1).min(16);
        self.first_delay.saturating_mul(factor).min(self.max_delay)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PushReport {
    pub acked: Vec<String>,
    pub failed: Vec<String>,
}

/// Whether a `members()` failure means "the membership is changing right
/// now" (retry later) rather than "this node is not a member" (stop).
fn membership_changing(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
}

/// Push the CURRENT local record to every member until each acks. Each
/// attempt re-reads the record (a newer write supersedes the one that
/// started the push) and the member list (a membership change restarts the
/// round on the new set; removed nodes are dropped, new ones added).
///
/// N1 (rev 4): while the membership is changing (`members()` fails with
/// `WouldBlock`, or a call answers `MembershipChanged`), the attempt is
/// spent waiting and the round goes on within the same attempt budget
/// (first push + 11 retries, 1-60 s backoff, about 6 minutes). The round
/// stops early only when this node is no longer a member. If the
/// membership never settles, the error is returned (never dropped) so the
/// caller logs it.
pub async fn push_all(
    store: &Store,
    link: &dyn PeerLink,
    policy: RetryPolicy,
) -> io::Result<PushReport> {
    // `None` until a settled member list was read at least once.
    let mut pending: Option<Vec<String>> = None;
    let mut acked: Vec<String> = Vec::new();
    for attempt in 0..policy.attempts.max(1) {
        if attempt > 0 {
            if pending.as_ref().is_some_and(Vec::is_empty) {
                break;
            }
            tokio::time::sleep(policy.delay(attempt)).await;
        }
        // Every attempt works on the member list of its own revision:
        // removed nodes drop out, joined ones are added, acks are kept.
        let (revision, members) = match link.members() {
            Ok(list) => list,
            // Changing: wait for the next attempt on the same budget.
            Err(error) if membership_changing(&error) => continue,
            // No longer a member: stop; nothing may be sent.
            Err(error) => return Err(error),
        };
        acked.retain(|node| members.contains(node));
        let targets: Vec<String> = members
            .into_iter()
            .filter(|node| !acked.contains(node))
            .collect();
        if targets.is_empty() {
            pending = Some(targets);
            break;
        }
        let copy = store::export_copy(store)?;
        let mut still = Vec::new();
        for node in targets {
            let result = if copy.credentials.is_some() && !link.credentials_sync(&node) {
                let mut plain = copy.clone();
                plain.credentials = None;
                link.push(revision, &node, &plain).await
            } else {
                link.push(revision, &node, &copy).await
            };
            match result {
                Ok(_) => acked.push(node),
                // Includes `MembershipChanged`: the next attempt re-reads.
                Err(_) => still.push(node),
            }
        }
        pending = Some(still);
    }
    let Some(failed) = pending else {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "成员版本在全部重试期间都在变化",
        ));
    };
    Ok(PushReport { acked, failed })
}

/// Pull once from every member and keep the greatest copy. Returns whether
/// the local record changed.
pub async fn pull_all(store: &Store, link: &dyn PeerLink) -> io::Result<bool> {
    let (revision, members) = link.members()?;
    let mut changed = false;
    for node in members {
        if let Ok(copy) = link.pull(revision, &node).await {
            // An invalid copy from one member does not stop the others.
            if let Ok(outcome) = store::accept_copy(store, &copy) {
                changed |= outcome.applied;
            }
        }
    }
    Ok(changed)
}

/// Why the receiving side refused (HTTP status for the peer route).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeRefusal {
    /// Not a committed member of this node's current revision (403).
    NotMember,
    /// Malformed or invalid copy (400).
    Invalid,
    /// Store failure (500).
    Store,
}

fn check_member(from: &str, members: &[String]) -> Result<(), ServeRefusal> {
    if from.is_empty() || !members.iter().any(|member| member == from) {
        return Err(ServeRefusal::NotMember);
    }
    Ok(())
}

/// The push receiver. `members` are the node ids of this node's current
/// membership revision (the signed router already refused anyone else).
pub fn serve_push(
    store: &Store,
    from: &str,
    members: &[String],
    body: &[u8],
) -> Result<PushAck, ServeRefusal> {
    check_member(from, members)?;
    let copy: OpenidCopy = serde_json::from_slice(body).map_err(|_| ServeRefusal::Invalid)?;
    store::accept_copy(store, &copy)
        .map(PushAck::from)
        .map_err(|error| match error.kind() {
            io::ErrorKind::InvalidData => ServeRefusal::Invalid,
            _ => ServeRefusal::Store,
        })
}

/// The pull receiver: this node's record, `unbound` included.
pub fn serve_pull(
    store: &Store,
    from: &str,
    members: &[String],
    credentials_sync: bool,
) -> Result<OpenidCopy, ServeRefusal> {
    check_member(from, members)?;
    let mut copy = store::export_copy(store).map_err(|_| ServeRefusal::Store)?;
    if !credentials_sync {
        // The same capability projection as push: old peers reject unknown
        // fields and must never receive an AppSecret they cannot sync.
        copy.credentials = None;
    }
    Ok(copy)
}

pub fn peer_supports_credentials(node: &str) -> bool {
    crate::cluster::cluster_state_read()
        .nodes
        .get(node)
        .and_then(|status| status.notify_credentials_sync)
        .unwrap_or(false)
}

/// Addendum C transport rule, sending side: the openid goes to a member
/// only over HTTPS, or plain HTTP to a loopback host on this machine
/// (`127.0.0.0/8`, `::1`, `localhost`; e.g. the authenticated tunnel).
/// Plain HTTP to any remote address is refused.
pub fn outbound_transport_allowed(api_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(api_url) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => match url.host_str() {
            Some(host) if host.eq_ignore_ascii_case("localhost") => true,
            Some(host) => host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()),
            None => false,
        },
        _ => false,
    }
}

/// Receiving side (Architect ruling, rev 2): the socket address is not
/// checked. `PeerEndpoint::parse` already limits members to HTTPS or plain
/// HTTP through an authenticated same-host tunnel, and every request is
/// signed, so a TLS proxy on another machine is fine. Any request with
/// `X-Forwarded-Proto: http` is rejected, whatever `X-Forwarded-For` says.
/// That header is advisory only, not a security boundary
/// (`X-Forwarded-Proto: http` with a non-loopback or missing
/// `X-Forwarded-For`).
pub fn inbound_transport_allowed(headers: &axum::http::HeaderMap) -> bool {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    // Forwarded headers never let a request through (rev 3).
    proto.as_deref() != Some("http")
}

/// Node ids of the committed manifest, only while this node is a current
/// member of it (`trust_snapshot`).
pub fn current_members(store: &Arc<Store>) -> io::Result<(u64, Vec<String>)> {
    use crate::cluster::membership::{trust_snapshot, Membership};
    let trust = trust_snapshot(store)?;
    let manifest = Membership::new(Arc::clone(store))
        .manifest()?
        .filter(|m| trust.revisions == [m.revision] && m.cluster_id == trust.cluster_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "成员版本正在变化"))?;
    Ok((
        manifest.revision,
        manifest.members.into_iter().map(|m| m.node_id).collect(),
    ))
}

/// The production link: signed ordinary peer calls resolved from the
/// committed manifest, one revision at a time.
pub struct SignedLink {
    store: Arc<Store>,
    local: String,
    timeout: Duration,
}

impl SignedLink {
    pub fn new(store: Arc<Store>, local: String) -> Self {
        Self {
            store,
            local,
            timeout: Duration::from_secs(10),
        }
    }

    async fn call(
        &self,
        revision: u64,
        node: &str,
        route: crate::cluster::peer_auth::RoutePolicy,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, LinkError> {
        let (status, body) = self.call_status(revision, node, route, body).await?;
        if !(200..300).contains(&status) {
            return Err(LinkError::Refused);
        }
        Ok(body)
    }

    async fn call_status(
        &self,
        revision: u64,
        node: &str,
        route: crate::cluster::peer_auth::RoutePolicy,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), LinkError> {
        let (now, _) = current_members(&self.store).map_err(|_| LinkError::MembershipChanged)?;
        if now != revision {
            return Err(LinkError::MembershipChanged);
        }
        let membership = crate::cluster::membership::Membership::new(Arc::clone(&self.store));
        let allowed = membership
            .manifest()
            .ok()
            .flatten()
            .and_then(|m| m.members.into_iter().find(|m| m.node_id == node))
            .is_some_and(|m| outbound_transport_allowed(&m.api_url));
        if !allowed {
            // Never put the openid on plain HTTP to a remote address.
            return Err(LinkError::Refused);
        }
        let client =
            crate::cluster::peer_call::peer_client().map_err(|_| LinkError::Unreachable)?;
        let response = crate::cluster::peer_call::send_ordinary_with(
            &membership,
            &client,
            node,
            route,
            body,
            self.timeout,
        )
        .await
        .map_err(|error| match crate::cluster::peer_call::classify(error) {
            crate::cluster::peer_call::PeerCallError::Unreachable(_) => LinkError::Unreachable,
            crate::cluster::peer_call::PeerCallError::Refused(_) => LinkError::Refused,
        })?;
        Ok((response.status, response.body))
    }
}

/// A forwarded save's answer: 2xx with the outcome, 409 with a fixed code,
/// anything else "unreachable" (nothing is applied locally either way).
pub(crate) fn forward_reply(
    status: u16,
    body: &[u8],
) -> Result<super::credentials::SaveOutcome, super::credentials::SaveError> {
    use super::credentials::SaveError;
    #[derive(Deserialize)]
    struct Reply {
        data: Option<serde_json::Value>,
    }
    let data = serde_json::from_slice::<Reply>(body)
        .ok()
        .and_then(|reply| reply.data);
    match status {
        200..=299 => data
            .and_then(|data| serde_json::from_value(data).ok())
            .ok_or(SaveError::HolderUnreachable),
        409 => {
            let code = data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let current = data
                .as_ref()
                .and_then(|d| d.get("current_generation"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Err(match code {
                "credentials_conflict" => SaveError::Conflict { current },
                _ => SaveError::HolderMoved,
            })
        }
        400 => Err(SaveError::Invalid),
        _ => Err(SaveError::HolderUnreachable),
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    data: Option<T>,
}

fn open<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, LinkError> {
    let envelope: Envelope<T> = serde_json::from_slice(body).map_err(|_| LinkError::Refused)?;
    match envelope {
        Envelope {
            success: true,
            data: Some(data),
        } => Ok(data),
        _ => Err(LinkError::Refused),
    }
}

#[async_trait]
impl PeerLink for SignedLink {
    fn clustered(&self) -> bool {
        use crate::cluster::membership::{Lifecycle, Membership};
        // Managed: push. Left: push_all stops with the "removed" warning.
        // Standalone and the pre-join states have no members at all.
        Membership::new(Arc::clone(&self.store))
            .local()
            .is_ok_and(|local| matches!(local.lifecycle, Lifecycle::Managed | Lifecycle::Left))
    }

    fn members(&self) -> io::Result<(u64, Vec<String>)> {
        let (revision, members) = current_members(&self.store)?;
        Ok((
            revision,
            members
                .into_iter()
                .filter(|node| *node != self.local)
                .collect(),
        ))
    }

    async fn push(
        &self,
        revision: u64,
        node: &str,
        copy: &OpenidCopy,
    ) -> Result<PushAck, LinkError> {
        let body =
            zeroize::Zeroizing::new(serde_json::to_vec(copy).map_err(|_| LinkError::Refused)?);
        let reply = self
            .call(
                revision,
                node,
                crate::cluster::peer_call::routes::NOTIFY_OPENID_PUSH,
                body.to_vec(),
            )
            .await?;
        open(&reply)
    }

    fn credentials_sync(&self, node: &str) -> bool {
        peer_supports_credentials(node)
    }

    async fn forward(
        &self,
        revision: u64,
        node: &str,
        save: &super::credentials::CredentialSave,
    ) -> Result<super::credentials::SaveOutcome, super::credentials::SaveError> {
        use super::credentials::SaveError;
        let body =
            zeroize::Zeroizing::new(serde_json::to_vec(save).map_err(|_| SaveError::Invalid)?);
        let (status, reply) = self
            .call_status(
                revision,
                node,
                crate::cluster::peer_call::routes::NOTIFY_CREDENTIALS_SAVE,
                body.to_vec(),
            )
            .await
            .map_err(|_| SaveError::HolderUnreachable)?;
        forward_reply(status, &reply)
    }

    async fn pull(&self, revision: u64, node: &str) -> Result<OpenidCopy, LinkError> {
        let reply = zeroize::Zeroizing::new(
            self.call(
                revision,
                node,
                crate::cluster::peer_call::routes::NOTIFY_OPENID_PULL,
                Vec::new(),
            )
            .await?,
        );
        open(&reply)
    }
}

/// N2 (rev 4): at most one push round runs per process, and at most one
/// follow-up is owed. A bind or unbind during a round only sets the
/// "push again" flag; however many arrive, they merge into ONE follow-up
/// round, which re-reads the record and so sends the latest state.
#[derive(Debug, Default)]
pub struct PushCoalescer {
    state: std::sync::Mutex<CoalescerState>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CoalescerState {
    running: bool,
    again: bool,
}

impl PushCoalescer {
    pub const fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(CoalescerState {
                running: false,
                again: false,
            }),
        }
    }

    /// A push is wanted. `true`: the caller starts the round loop. `false`:
    /// a round is running and will run once more after it.
    pub fn request(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.running {
            state.again = true;
            false
        } else {
            state.running = true;
            state.again = false;
            true
        }
    }

    /// A round ended. `true`: run one more (the flag was set, and is now
    /// cleared). `false`: the loop is done and the next request starts one.
    pub fn finish_round(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.again {
            state.again = false;
            true
        } else {
            state.running = false;
            false
        }
    }

    /// Run rounds until no follow-up is owed. Call only after `request()`
    /// returned `true`.
    ///
    /// F2 (Clerk, 1b gate): a round that panics is caught. The "push again"
    /// flag set during it is kept, and a new round starts right away, so a
    /// bind/unbind/credential save that arrived meanwhile is still pushed.
    /// Without the flag the loop ends and the next request starts a new
    /// one. If the task itself is cancelled, the guard clears both flags.
    pub async fn drive<F, Fut>(&self, mut round: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let mut guard = RunningGuard {
            coalescer: self,
            armed: true,
        };
        loop {
            let caught =
                futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(round())).await;
            if caught.is_err() {
                tracing::warn!("QQ 通知同步本轮异常结束，有待推送的变更时会立即重新推送");
            }
            if !self.finish_round() {
                // finish_round already cleared `running`; a new loop may
                // start right away, so the guard must not touch it again.
                guard.armed = false;
                break;
            }
        }
    }
}

struct RunningGuard<'a> {
    coalescer: &'a PushCoalescer,
    armed: bool,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self
                .coalescer
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.running = false;
            state.again = false;
        }
    }
}

static PUSH_COALESCER: PushCoalescer = PushCoalescer::new();

/// Log one round's outcome. Node ids and an error kind only; never the
/// openid and never peer text.
fn log_round(result: &io::Result<PushReport>) {
    match result {
        Ok(report) if report.failed.is_empty() => {
            if !report.acked.is_empty() {
                tracing::info!(
                    "QQ 通知管理员 openid 已同步到 {} 个节点 (openid=<set>)",
                    report.acked.len()
                );
            }
        }
        Ok(report) => tracing::warn!(
            "QQ 通知管理员 openid 未能同步到节点 {}，它们会在下次拉取时更新",
            report.failed.join(", ")
        ),
        Err(error) if membership_changing(error) => tracing::warn!(
            "QQ 通知管理员 openid 本轮同步失败：成员版本在全部重试期间都在变化，其他节点会在下次拉取时更新"
        ),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => tracing::warn!(
            "QQ 通知管理员 openid 本轮同步已停止：本节点已不在当前成员清单"
        ),
        Err(error) => tracing::warn!(
            "QQ 通知管理员 openid 本轮同步失败 ({:?})，其他节点会在下次拉取时更新",
            error.kind()
        ),
    }
}

/// One push round with its log line. On a single-node install there is no
/// one to push to: nothing is called and nothing is logged (R4-1).
pub async fn push_round(store: &Store, link: &dyn PeerLink, policy: RetryPolicy) {
    if !link.clustered() {
        return;
    }
    let result = push_all(store, link, policy).await;
    log_round(&result);
}

/// After a bind, a change or an unbind on the writer: push in the
/// background. Without a managed membership this does nothing. A request
/// during a running round merges into one follow-up round.
pub fn spawn_push(store: Arc<Store>, local: String) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if !PUSH_COALESCER.request() {
        return;
    }
    tokio::spawn(async move {
        PUSH_COALESCER
            .drive(|| {
                let store = Arc::clone(&store);
                let local = local.clone();
                async move {
                    let link = SignedLink::new(Arc::clone(&store), local);
                    push_round(&store, &link, RetryPolicy::PRODUCTION).await;
                }
            })
            .await;
    });
}

/// After joining, a restart into a cluster or becoming the writer: pull
/// once. A change reloads the worker.
pub fn spawn_pull(store: Arc<Store>, local: String) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let link = SignedLink::new(Arc::clone(&store), local);
        if let Ok(true) = pull_all(&store, &link).await {
            tracing::info!("QQ 通知管理员 openid 已从其他节点同步 (openid=<set>)");
            let _ = super::send_command(super::Command::Reload);
        }
    });
}

#[cfg(test)]
#[path = "openid_sync_tests.rs"]
mod tests;
