//! Loopback stand-in for the QQ token and OpenAPI hosts (tests only).

use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct MockState {
    pub token_calls: usize,
    /// `(openid, body)` per message call.
    pub messages: Vec<(String, Value)>,
    /// Scripted replies for message calls; empty → success.
    pub replies: VecDeque<(u16, Value)>,
    pub token_reply: Option<(u16, Value)>,
    pub token_replies: VecDeque<(u16, Value)>,
    /// Delay a captured request to exercise cancellation and caller isolation.
    pub message_delay: std::time::Duration,
    /// What `/gateway` returns; defaults to a closed loopback port.
    pub gateway_url: Option<String>,
    /// Token-specific discovery refusals and observed Authorization values.
    pub gateway_refusals: std::collections::HashMap<String, (u16, Value)>,
    pub gateway_tokens: Vec<String>,
}

pub struct MockQq {
    pub base: String,
    pub state: Arc<Mutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockQq {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockQq {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(MockState::default()));
        let app = Router::new()
            .route("/app/getAppAccessToken", post(token))
            .route("/v2/users/{openid}/messages", post(message))
            .route("/gateway", get(gateway))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { base, state, task }
    }

    pub fn token_url(&self) -> String {
        format!("{}/app/getAppAccessToken", self.base)
    }

    pub fn script(&self, replies: impl IntoIterator<Item = (u16, Value)>) {
        self.state.lock().unwrap().replies.extend(replies);
    }

    pub fn messages(&self) -> Vec<(String, Value)> {
        self.state.lock().unwrap().messages.clone()
    }
}

async fn token(
    State(state): State<Arc<Mutex<MockState>>>,
) -> (axum::http::StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.token_calls += 1;
    let (status, body) = state
        .token_replies
        .pop_front()
        .or_else(|| state.token_reply.clone())
        .unwrap_or((
            200,
            json!({"access_token": "synthetic-token", "expires_in": "7200"}),
        ));
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        Json(body),
    )
}

async fn message(
    State(state): State<Arc<Mutex<MockState>>>,
    Path(openid): Path<String>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let (status, body, delay) = {
        let mut state = state.lock().unwrap();
        state.messages.push((openid, body));
        let (status, body) = state
            .replies
            .pop_front()
            .unwrap_or((200, json!({"id": "msg-1", "timestamp": 1})));
        (status, body, state.message_delay)
    };
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        Json(body),
    )
}

async fn gateway(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: axum::http::HeaderMap,
) -> (axum::http::StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    state.gateway_tokens.push(token.clone());
    if let Some((status, body)) = state.gateway_refusals.get(&token) {
        return (
            axum::http::StatusCode::from_u16(*status).unwrap(),
            Json(body.clone()),
        );
    }
    let url = state
        .gateway_url
        .clone()
        .unwrap_or_else(|| "ws://127.0.0.1:9/".to_string());
    (axum::http::StatusCode::OK, Json(json!({ "url": url })))
}

/// In-process stand-in for the signed peer channel (batch 1b tests): every
/// node is a `Store`, calls go through the real receivers
/// (`openid_sync::serve_push` / `serve_pull`) with the receiver's member
/// list, and a node can be made unreachable for its next N calls.
/// `(push target, changing reads, joining node, its store)`.
pub type ChangeOnPush = (String, u32, String, Arc<crate::storage::Store>);

pub struct MemCluster {
    pub revision: Mutex<u64>,
    /// Committed members of `revision`.
    pub members: Mutex<Vec<String>>,
    pub stores: Mutex<std::collections::HashMap<String, Arc<crate::storage::Store>>>,
    /// node → calls still to fail as unreachable (`u32::MAX` = offline).
    pub down: Mutex<std::collections::HashMap<String, u32>>,
    /// node → push calls it received (delivered or not).
    pub pushes: Mutex<std::collections::HashMap<String, u32>>,
    /// `members()` calls still to fail as "membership changing" (rev 4).
    pub changing: Mutex<u32>,
    /// Rev 4: when a push reaches this node, a membership change starts
    /// mid-round: that push answers `MembershipChanged`, the next
    /// `changing` reads fail, and the new revision adds the given member.
    pub change_on_push: Mutex<Option<ChangeOnPush>>,
    /// `false`: a single-node install (R4-1).
    pub clustered: Mutex<bool>,
    /// `members()` reads (any peer activity starts with one).
    pub member_reads: Mutex<u32>,
    /// Addendum F: nodes on an older build (no `credentials_sync`); they
    /// refuse a copy that carries the set, as `deny_unknown_fields` does.
    pub legacy: Mutex<std::collections::HashSet<String>>,
    /// Nodes that do not advertise `watch_sync`. A copy that still carries
    /// the follow list is refused, the same way an old decoder would.
    pub no_watch: Mutex<std::collections::HashSet<String>>,
    /// Addendum F: the node that holds the QQ gateway.
    pub holder: Mutex<Option<String>>,
}

impl MemCluster {
    pub fn new(nodes: &[(&str, Arc<crate::storage::Store>)]) -> Arc<Self> {
        Arc::new(Self {
            revision: Mutex::new(1),
            members: Mutex::new(nodes.iter().map(|(id, _)| id.to_string()).collect()),
            stores: Mutex::new(
                nodes
                    .iter()
                    .map(|(id, store)| (id.to_string(), Arc::clone(store)))
                    .collect(),
            ),
            down: Mutex::new(Default::default()),
            pushes: Mutex::new(Default::default()),
            changing: Mutex::new(0),
            change_on_push: Mutex::new(None),
            clustered: Mutex::new(true),
            member_reads: Mutex::new(0),
            legacy: Mutex::new(Default::default()),
            no_watch: Mutex::new(Default::default()),
            holder: Mutex::new(None),
        })
    }

    pub fn link(self: &Arc<Self>, local: &str) -> MemLink {
        MemLink {
            cluster: Arc::clone(self),
            local: local.to_string(),
        }
    }

    pub fn set_down(&self, node: &str, calls: u32) {
        self.down.lock().unwrap().insert(node.to_string(), calls);
    }

    /// Commit a new membership revision without `node`.
    pub fn remove(&self, node: &str) {
        self.members.lock().unwrap().retain(|member| member != node);
        *self.revision.lock().unwrap() += 1;
    }

    /// Commit a new membership revision that adds `node`.
    pub fn add(&self, node: &str, store: Arc<crate::storage::Store>) {
        self.stores.lock().unwrap().insert(node.to_string(), store);
        self.members.lock().unwrap().push(node.to_string());
        *self.revision.lock().unwrap() += 1;
    }

    fn reachable(&self, node: &str) -> bool {
        let mut down = self.down.lock().unwrap();
        match down.get_mut(node) {
            Some(0) | None => true,
            Some(left) => {
                if *left != u32::MAX {
                    *left -= 1;
                }
                false
            }
        }
    }

    fn store(&self, node: &str) -> Arc<crate::storage::Store> {
        Arc::clone(&self.stores.lock().unwrap()[node])
    }
}

pub struct MemLink {
    cluster: Arc<MemCluster>,
    local: String,
}

use super::openid_sync::{LinkError, PeerLink, PushAck};
use super::store::OpenidCopy;

#[async_trait::async_trait]
impl PeerLink for MemLink {
    fn clustered(&self) -> bool {
        *self.cluster.clustered.lock().unwrap()
    }

    fn members(&self) -> std::io::Result<(u64, Vec<String>)> {
        *self.cluster.member_reads.lock().unwrap() += 1;
        {
            let mut changing = self.cluster.changing.lock().unwrap();
            if *changing > 0 {
                *changing -= 1;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "成员版本正在变化",
                ));
            }
        }
        let members = self.cluster.members.lock().unwrap().clone();
        if !members.contains(&self.local) {
            // Removed: no trust snapshot, so no peer calls at all.
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "本节点不在当前成员清单",
            ));
        }
        Ok((
            *self.cluster.revision.lock().unwrap(),
            members.into_iter().filter(|m| *m != self.local).collect(),
        ))
    }

    async fn push(
        &self,
        revision: u64,
        node: &str,
        copy: &OpenidCopy,
    ) -> Result<PushAck, LinkError> {
        if revision != *self.cluster.revision.lock().unwrap() {
            return Err(LinkError::MembershipChanged);
        }
        *self
            .cluster
            .pushes
            .lock()
            .unwrap()
            .entry(node.to_string())
            .or_default() += 1;
        let hook = {
            let mut hook = self.cluster.change_on_push.lock().unwrap();
            if hook.as_ref().is_some_and(|(at, ..)| at == node) {
                hook.take()
            } else {
                None
            }
        };
        if let Some((_, reads, joined, store)) = hook {
            *self.cluster.changing.lock().unwrap() = reads;
            self.cluster.add(&joined, store);
            return Err(LinkError::MembershipChanged);
        }
        if !self.cluster.reachable(node) {
            return Err(LinkError::Unreachable);
        }
        let members = self.cluster.members.lock().unwrap().clone();
        if copy.credentials.is_some() && self.cluster.legacy.lock().unwrap().contains(node) {
            return Err(LinkError::Refused);
        }
        if copy.watch.is_some() && self.cluster.no_watch.lock().unwrap().contains(node) {
            return Err(LinkError::Refused);
        }
        let body = serde_json::to_vec(copy).unwrap();
        super::openid_sync::serve_push(&self.cluster.store(node), &self.local, &members, &body)
            .map_err(|_| LinkError::Refused)
    }

    fn credentials_sync(&self, node: &str) -> bool {
        !self.cluster.legacy.lock().unwrap().contains(node)
    }

    fn watch_sync(&self, node: &str) -> bool {
        !self.cluster.no_watch.lock().unwrap().contains(node)
    }

    async fn forward(
        &self,
        revision: u64,
        node: &str,
        save: &super::credentials::CredentialSave,
    ) -> Result<super::credentials::SaveOutcome, super::credentials::SaveError> {
        use super::credentials::{ForwardRefusal, SaveError};
        if revision != *self.cluster.revision.lock().unwrap() || !self.cluster.reachable(node) {
            return Err(SaveError::HolderUnreachable);
        }
        let members = self.cluster.members.lock().unwrap().clone();
        let holds = self.cluster.holder.lock().unwrap().as_deref() == Some(node);
        let body = serde_json::to_vec(save).unwrap();
        super::credentials::serve_forward(
            &self.cluster.store(node),
            &self.local,
            &members,
            &body,
            holds,
            node,
            1_791_702_400,
        )
        .map_err(|refusal| match refusal {
            ForwardRefusal::NotMember => SaveError::HolderUnreachable,
            ForwardRefusal::Save(error) => error,
        })
    }

    async fn pull(&self, revision: u64, node: &str) -> Result<OpenidCopy, LinkError> {
        if revision != *self.cluster.revision.lock().unwrap() {
            return Err(LinkError::MembershipChanged);
        }
        if !self.cluster.reachable(node) {
            return Err(LinkError::Unreachable);
        }
        let members = self.cluster.members.lock().unwrap().clone();
        let supported = !self.cluster.legacy.lock().unwrap().contains(&self.local);
        let watch = !self.cluster.no_watch.lock().unwrap().contains(&self.local);
        let copy = super::openid_sync::serve_pull(
            &self.cluster.store(node),
            &self.local,
            &members,
            supported,
            watch,
        )
        .map_err(|_| LinkError::Refused)?;
        // Through JSON, as on the wire.
        Ok(serde_json::from_slice(&serde_json::to_vec(&copy).unwrap()).unwrap())
    }
}
