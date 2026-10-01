//! Managed membership: browser operations and the signed v1 peer routes.
//! Passwords are consumed by a single call and never echoed, stored or logged.

use super::*;
use crate::cluster::membership::{
    member_endpoint, validate_operation_id, Change, DecisionKind, Descriptor, Finish,
    ForwardRequest, Lifecycle, Operation, OperationStatus, PairingRequest, Proposal, Reservation,
    Runtime, SetupRequest, StatusRequest, LOCAL_RECORD,
};
use crate::cluster::peer_call::{routes, send_ordinary_with, AuthenticatedNode};
use crate::webui::listen::{AuthGeneration, AuthState};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path};
use axum::http::HeaderMap;
use axum::Extension;
use std::net::SocketAddr;
use std::sync::Arc;

fn reply<T: Serialize>(
    status: StatusCode,
    data: Option<T>,
    message: impl Into<String>,
) -> Response {
    let message = message.into();
    (
        status,
        Json(ApiResponse {
            success: status.is_success(),
            data,
            message: (!message.is_empty()).then_some(message),
        }),
    )
        .into_response()
}

fn error_reply(error: &std::io::Error) -> Response {
    let status = match error.kind() {
        std::io::ErrorKind::WouldBlock => StatusCode::CONFLICT,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            StatusCode::BAD_REQUEST
        }
        std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    reply::<()>(status, None, error.to_string())
}

fn forbidden_origin() -> Response {
    reply::<()>(
        StatusCode::FORBIDDEN,
        None,
        "请求来源或登录状态已失效，请刷新页面",
    )
}

#[derive(Serialize)]
pub struct LocalView {
    member_id: Option<String>,
    node_id: String,
    name: String,
    api_url: String,
    priority: i32,
}

#[derive(Serialize)]
pub struct MemberView {
    member_id: String,
    node_id: String,
    name: String,
    api_url: String,
    priority: i32,
    fingerprint: String,
}

#[derive(Serialize)]
pub struct MembershipView {
    protocol: u32,
    lifecycle: Lifecycle,
    local_revision: u64,
    password_required: bool,
    local: LocalView,
    cluster_id: Option<String>,
    revision: u64,
    digest: Option<String>,
    members: Vec<MemberView>,
    public_member_id: Option<String>,
    operation: Option<OperationStatus>,
}

fn fingerprint(member_id: &str) -> String {
    member_id
        .as_bytes()
        .chunks(4)
        .take(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

fn project(runtime: &Runtime, op: &Operation) -> OperationStatus {
    let mut status = op.status();
    if !status.terminal && status.message.is_some() && !runtime.driver_running(&status.operation_id)
    {
        status.phase = "needs_attention".into();
        status.retryable = true;
    }
    status
}

fn membership_view(runtime: &Runtime, password_required: bool) -> std::io::Result<MembershipView> {
    let membership = &runtime.membership;
    let store = membership.store();
    let local = membership.local()?;
    let manifest = membership.manifest()?;
    let local_view = match &local.descriptor {
        Some(d) => LocalView {
            member_id: Some(d.member_id.clone()),
            node_id: d.node_id.clone(),
            name: d.name.clone(),
            api_url: d.api_url.clone(),
            priority: d.priority,
        },
        None => {
            let cluster = store
                .read("config.json")?
                .map(|doc| doc.value["cluster"].clone())
                .unwrap_or_default();
            LocalView {
                member_id: None,
                node_id: cluster["node_id"].as_str().unwrap_or_default().into(),
                name: cluster["node_name"].as_str().unwrap_or_default().into(),
                api_url: cluster["public_api_url"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                priority: cluster["priority"].as_i64().unwrap_or_default() as i32,
            }
        }
    };
    let operation = membership
        .operations()?
        .into_iter()
        .find(|op| !op.terminal() || !op.finished_locally)
        .map(|op| project(runtime, &op));
    let managed = matches!(local.lifecycle, Lifecycle::Managed);
    let manifest = manifest.filter(|_| managed);
    Ok(MembershipView {
        protocol: crate::cluster::membership::PROTOCOL,
        lifecycle: local.lifecycle,
        local_revision: store.revision(LOCAL_RECORD)?.unwrap_or(0),
        password_required,
        local: local_view,
        cluster_id: manifest.as_ref().map(|m| m.cluster_id.clone()),
        revision: manifest.as_ref().map_or(0, |m| m.revision),
        digest: manifest.as_ref().map(|m| m.digest.clone()),
        members: manifest
            .as_ref()
            .map(|m| {
                m.members
                    .iter()
                    .map(|d| MemberView {
                        member_id: d.member_id.clone(),
                        node_id: d.node_id.clone(),
                        name: d.name.clone(),
                        api_url: d.api_url.clone(),
                        priority: d.priority,
                        fingerprint: fingerprint(&d.member_id),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        public_member_id: manifest.and_then(|m| m.public_member_id),
        operation,
    })
}

pub(crate) async fn get_membership(Extension(auth): Extension<Arc<AuthState>>) -> Response {
    match membership_view(auth.runtime(), auth.password_configured()) {
        Ok(view) => reply(StatusCode::OK, Some(view), ""),
        Err(error) => error_reply(&error),
    }
}

async fn setup(
    auth: Arc<AuthState>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    body: Bytes,
    create: bool,
) -> Response {
    if !auth.browser_mutation_allowed(&headers, generation.map(|Extension(g)| g)) {
        return forbidden_origin();
    }
    let Ok(request) = serde_json::from_slice::<SetupRequest>(&body) else {
        return reply::<()>(StatusCode::BAD_REQUEST, None, "请求格式无效");
    };
    if !auth.password_configured() {
        return reply::<()>(
            StatusCode::CONFLICT,
            None,
            "请先在 系统设置 → 安全 中设置面板密码",
        );
    }
    let runtime = auth.runtime();
    if let Err(error) = runtime.setup(request, create).await {
        return error_reply(&error);
    }
    let message = if create {
        "已创建集群，本服务器是唯一成员"
    } else {
        "本服务器已准备加入，请在集群中任一服务器的面板添加它"
    };
    match membership_view(runtime, true) {
        Ok(view) => reply(StatusCode::OK, Some(view), message),
        Err(error) => error_reply(&error),
    }
}

pub(crate) async fn create_cluster(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    setup(auth, generation, headers, body, true).await
}

pub(crate) async fn prepare_join(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    setup(auth, generation, headers, body, false).await
}

/// Never derive Debug: `target_password` must not reach logs.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OperationBody {
    Add {
        operation_id: String,
        expected_revision: u64,
        target_url: String,
        target_password: String,
    },
    Remove {
        operation_id: String,
        expected_revision: u64,
        target_member_id: String,
        #[serde(default)]
        replacement_public_member_id: Option<String>,
    },
    UpdateNode {
        operation_id: String,
        expected_revision: u64,
        target_member_id: String,
        name: String,
        api_url: String,
        priority: i32,
    },
    SetPublicNode {
        operation_id: String,
        expected_revision: u64,
        public_member_id: Option<String>,
    },
}

fn status_reply(runtime: &Runtime, id: &str, message: &str) -> Response {
    match runtime.membership.operation(id) {
        Ok(Some(op)) => reply(StatusCode::OK, Some(project(runtime, &op)), message),
        Ok(None) => reply::<()>(StatusCode::OK, None, message),
        Err(error) => error_reply(&error),
    }
}

pub(crate) async fn submit_operation(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !auth.browser_mutation_allowed(&headers, generation.map(|Extension(g)| g)) {
        return forbidden_origin();
    }
    let Ok(body) = serde_json::from_slice::<OperationBody>(&body) else {
        return reply::<()>(StatusCode::BAD_REQUEST, None, "请求格式无效");
    };
    let runtime = auth.runtime().clone();
    let (id, expected, change, password) = match body {
        OperationBody::Add {
            operation_id,
            expected_revision,
            target_url,
            target_password,
        } => {
            if member_endpoint(&target_url).is_err() {
                return reply::<()>(
                    StatusCode::BAD_REQUEST,
                    None,
                    "目标地址必须是 HTTPS，或明确的本机回环认证隧道地址",
                );
            }
            if target_password.trim().is_empty() {
                return reply::<()>(StatusCode::BAD_REQUEST, None, "请输入目标服务器的面板密码");
            }
            (
                operation_id,
                expected_revision,
                Change::Add { target_url },
                Some(target_password),
            )
        }
        OperationBody::Remove {
            operation_id,
            expected_revision,
            target_member_id,
            replacement_public_member_id,
        } => (
            operation_id,
            expected_revision,
            Change::Remove {
                target_member_id,
                replacement_public_member_id,
            },
            None,
        ),
        OperationBody::UpdateNode {
            operation_id,
            expected_revision,
            target_member_id,
            name,
            api_url,
            priority,
        } => (
            operation_id,
            expected_revision,
            Change::UpdateNode {
                target_member_id,
                name,
                api_url,
                priority,
            },
            None,
        ),
        OperationBody::SetPublicNode {
            operation_id,
            expected_revision,
            public_member_id,
        } => (
            operation_id,
            expected_revision,
            Change::SetPublicNode { public_member_id },
            None,
        ),
    };
    if validate_operation_id(&id).is_err() {
        return reply::<()>(StatusCode::BAD_REQUEST, None, "操作标识格式无效");
    }
    let membership = &runtime.membership;
    let (me, manifest) = match (
        membership.identity(),
        membership.manifest(),
        membership.local(),
    ) {
        (Ok(identity), Ok(Some(manifest)), Ok(local)) if local.lifecycle == Lifecycle::Managed => {
            (identity.public().member_id.clone(), manifest)
        }
        _ => return reply::<()>(StatusCode::CONFLICT, None, "本服务器尚未加入受管集群"),
    };
    // A repeated submission continues the recorded operation; it never
    // resends a password or starts a substitute.
    if let Ok(Some(op)) = membership.operation(&id) {
        if op.intent.change != change || op.intent.base.revision != expected {
            return reply::<()>(StatusCode::CONFLICT, None, "同一操作标识不能更改内容");
        }
        if op.intent.coordinator == me {
            runtime.spawn_driver(id.clone());
        }
        return status_reply(&runtime, &id, "操作已在进行，正在继续");
    }
    if expected != manifest.revision {
        return reply::<()>(StatusCode::CONFLICT, None, "成员版本已改变，请刷新后重试");
    }
    if matches!(&change, Change::Remove { target_member_id, .. } if *target_member_id == me) {
        return forward_self_removal(&runtime, &manifest, id, expected, change).await;
    }
    let desired: Vec<Descriptor> = match &change {
        Change::Remove {
            target_member_id, ..
        } => manifest
            .members
            .iter()
            .filter(|m| &m.member_id != target_member_id)
            .cloned()
            .collect(),
        _ => manifest.members.clone(),
    };
    if let Err(missing) = runtime.preflight(&manifest, &desired).await {
        return reply::<()>(
            StatusCode::BAD_GATEWAY,
            None,
            format!("以下服务器必须在线才能变更成员: {}", missing.join("、")),
        );
    }
    let begin = {
        let (id, me, change) = (id.clone(), me.clone(), change.clone());
        let membership = membership.clone();
        tokio::task::spawn_blocking(move || membership.begin(id, expected, me, change)).await
    };
    let op = match begin {
        Ok(Ok(op)) => op,
        Ok(Err(error)) => return error_reply(&error),
        Err(_) => return reply::<()>(StatusCode::INTERNAL_SERVER_ERROR, None, "成员事务失败"),
    };
    let Some(password) = password else {
        let result = {
            let (membership, id) = (membership.clone(), id.clone());
            tokio::task::spawn_blocking(move || membership.propose(id, None)).await
        };
        if let Ok(Err(error)) = result {
            return error_reply(&error);
        }
        runtime.spawn_driver(id.clone());
        return status_reply(&runtime, &id, "成员操作已开始");
    };
    add_with_password(&runtime, &op, password).await
}

async fn record(
    runtime: &Runtime,
    f: impl FnOnce(&crate::cluster::membership::Membership) -> std::io::Result<()> + Send + 'static,
) -> std::io::Result<()> {
    let membership = runtime.membership.clone();
    tokio::task::spawn_blocking(move || f(&membership))
        .await
        .map_err(|_| std::io::Error::other("成员事务失败"))?
}

async fn abort(runtime: &Runtime, id: &str, message: String, candidate: Option<String>) {
    let id = id.to_owned();
    let result = record(runtime, move |m| {
        m.decide(id.clone(), DecisionKind::Abort)?;
        if let Some(candidate) = candidate {
            m.delivered(id.clone(), candidate)?;
        }
        m.note(id, message)
    })
    .await;
    if let Err(error) = result {
        tracing::warn!("成员操作取消记录失败: {error}");
    }
}

async fn add_with_password(runtime: &Runtime, op: &Operation, password: String) -> Response {
    let id = op.intent.operation_id.clone();
    match runtime.reserve_remote(op, password).await {
        Ok((_, Reservation::Reserved(receipt))) => {
            let proposal_id = id.clone();
            if let Err(error) = record(runtime, move |m| {
                m.propose(proposal_id, Some(receipt)).map(|_| ())
            })
            .await
            {
                return error_reply(&error);
            }
            runtime.spawn_driver(id.clone());
            status_reply(runtime, &id, "目标服务器已确认配对，成员操作已开始")
        }
        Ok((identity, Reservation::Refused(code, message))) => {
            abort(runtime, &id, message.clone(), Some(identity.member_id)).await;
            runtime.spawn_driver(id.clone());
            let status = match code {
                403 => StatusCode::FORBIDDEN,
                429 => StatusCode::TOO_MANY_REQUESTS,
                400 => StatusCode::BAD_REQUEST,
                _ => StatusCode::CONFLICT,
            };
            reply::<()>(status, None, message)
        }
        Ok((_, Reservation::Uncertain(message))) => {
            let note = format!("无法确认目标服务器是否已预约，将自动核对: {message}");
            let note_id = id.clone();
            let _ = record(runtime, move |m| m.note(note_id, note)).await;
            runtime.spawn_driver(id.clone());
            status_reply(runtime, &id, "目标服务器响应丢失，正在核对配对状态")
        }
        Err(error) => {
            abort(
                runtime,
                &id,
                "目标服务器不可达或身份验证失败，操作已取消".into(),
                None,
            )
            .await;
            runtime.spawn_driver(id.clone());
            reply::<()>(
                StatusCode::BAD_GATEWAY,
                None,
                format!("目标服务器不可达或身份验证失败: {error}"),
            )
        }
    }
}

/// The coordinator must remain a member, so self-removal is coordinated by a
/// retained node. This panel keeps the same operation ID for status.
async fn forward_self_removal(
    runtime: &Runtime,
    manifest: &crate::cluster::membership::Manifest,
    id: String,
    expected: u64,
    change: Change,
) -> Response {
    let me = runtime
        .membership
        .identity()
        .map(|i| i.public().member_id.clone())
        .unwrap_or_default();
    let mut others: Vec<&Descriptor> = manifest
        .members
        .iter()
        .filter(|m| m.member_id != me)
        .collect();
    others.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.node_id.cmp(&b.node_id)));
    let request = ForwardRequest {
        operation_id: id,
        expected_revision: expected,
        change,
    };
    let Ok(body) = serde_json::to_vec(&request) else {
        return reply::<()>(StatusCode::INTERNAL_SERVER_ERROR, None, "请求编码失败");
    };
    let mut unreachable = Vec::new();
    for member in others {
        match send_ordinary_with(
            &runtime.membership,
            &runtime.client,
            &member.node_id,
            routes::FORWARD,
            body.clone(),
            std::time::Duration::from_secs(15),
        )
        .await
        {
            Ok(response) if response.status == 200 => {
                let status: Option<OperationStatus> = serde_json::from_slice(&response.body).ok();
                return reply(
                    StatusCode::OK,
                    status,
                    format!("已交由 {} 协调移除本服务器", member.node_id),
                );
            }
            Ok(response) if response.status == 409 => {
                return reply::<()>(
                    StatusCode::CONFLICT,
                    None,
                    String::from_utf8_lossy(&response.body)
                        .chars()
                        .take(200)
                        .collect::<String>(),
                )
            }
            _ => unreachable.push(member.node_id.clone()),
        }
    }
    reply::<()>(
        StatusCode::BAD_GATEWAY,
        None,
        format!("没有可协调的保留服务器: {}", unreachable.join("、")),
    )
}

pub(crate) async fn get_operation(
    Extension(auth): Extension<Arc<AuthState>>,
    Path(id): Path<String>,
) -> Response {
    operation_status(auth.runtime(), id, false).await
}

pub(crate) async fn retry_operation(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !auth.browser_mutation_allowed(&headers, generation.map(|Extension(g)| g)) {
        return forbidden_origin();
    }
    operation_status(auth.runtime(), id, true).await
}

async fn operation_status(runtime: &Runtime, id: String, retry: bool) -> Response {
    if validate_operation_id(&id).is_err() {
        return reply::<()>(StatusCode::BAD_REQUEST, None, "操作标识格式无效");
    }
    let op = match runtime.membership.operation(&id) {
        Ok(Some(op)) => op,
        Ok(None) => {
            return reply::<()>(
                StatusCode::NOT_FOUND,
                None,
                "操作尚未到达本服务器，请稍后刷新或在协调服务器查看",
            )
        }
        Err(error) => return error_reply(&error),
    };
    let me = runtime
        .membership
        .identity()
        .map(|i| i.public().member_id.clone())
        .unwrap_or_default();
    if op.intent.coordinator == me {
        if retry {
            runtime.spawn_driver(id.clone());
        }
        return status_reply(runtime, &id, "");
    }
    if !op.finished_locally {
        if let Some(status) = runtime.query_coordinator(&op, retry).await {
            return reply(StatusCode::OK, Some(status), "");
        }
        let mut status = project(runtime, &op);
        status.phase = "needs_attention".into();
        status.message = Some(format!(
            "无法连接协调服务器 {}，本服务器保持暂停直到结果确认",
            status.coordinator_node_id
        ));
        return reply(StatusCode::OK, Some(status), "");
    }
    status_reply(runtime, &id, "")
}

// ----- signed peer routes -----

fn peer_error(error: std::io::Error) -> Response {
    let status = match error.kind() {
        std::io::ErrorKind::WouldBlock => StatusCode::CONFLICT,
        std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, error.to_string()).into_response()
}

fn peer_json<T: Serialize>(result: std::io::Result<T>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => peer_error(error),
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Option<T> {
    serde_json::from_slice(body).ok()
}

fn bad_request() -> Response {
    (StatusCode::BAD_REQUEST, "请求格式无效").into_response()
}

/// Bootstrap identity proof. Reads an existing identity, never creates one.
pub(crate) async fn peer_hello(
    Extension(auth): Extension<Arc<AuthState>>,
    body: Bytes,
) -> Response {
    let request = match parse::<crate::cluster::peer_auth::HelloRequest>(&body) {
        Some(request) => request,
        None => return bad_request(),
    };
    let Ok(identity) = auth.runtime().membership.identity() else {
        return (StatusCode::NOT_FOUND, "本服务器尚未创建或准备加入集群").into_response();
    };
    peer_json(auth.peer_hello(&identity, &request))
}

pub(crate) async fn peer_reserve(
    Extension(auth): Extension<Arc<AuthState>>,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Bytes,
) -> Response {
    let request = match parse::<PairingRequest>(&body) {
        Some(request) => request,
        None => return bad_request(),
    };
    let source = connection.map_or(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        |Extension(ConnectInfo(addr))| addr.ip(),
    );
    let PairingRequest { intent, password } = request;
    let password = zeroize::Zeroizing::new(password);
    let revision = match auth.check_peer_password(source, &password) {
        Ok(revision) => revision,
        Err((StatusCode::TOO_MANY_REQUESTS, retry)) => return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::RETRY_AFTER,
                retry.map_or(60, |d| d.as_secs().max(1)).to_string(),
            )],
            Json(
                serde_json::json!({"success":false,"message":"目标服务器密码尝试过多，请稍后重试"}),
            ),
        )
            .into_response(),
        Err((status, _)) => {
            return (
                status,
                Json(
                    serde_json::json!({"success":false,"message":"目标服务器面板密码错误或未设置"}),
                ),
            )
                .into_response()
        }
    };
    let result = auth
        .runtime()
        .handle_reserve(intent, move |tx| {
            if tx.read("webui-password")?.map(|doc| doc.revision) != revision {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "目标服务器面板密码已更改",
                ));
            }
            Ok(())
        })
        .await;
    match result {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => {
            let status = match error.kind() {
                std::io::ErrorKind::WouldBlock => StatusCode::CONFLICT,
                std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                    StatusCode::BAD_REQUEST
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(serde_json::json!({"success":false,"message":error.to_string()})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn peer_prepare(
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(node): Extension<AuthenticatedNode>,
    body: Bytes,
) -> Response {
    match parse::<Proposal>(&body) {
        Some(proposal) => peer_json(
            auth.runtime()
                .handle_prepare(&node.member_id, proposal)
                .await,
        ),
        None => bad_request(),
    }
}

async fn peer_decide(
    auth: Arc<AuthState>,
    node: AuthenticatedNode,
    body: Bytes,
    kind: DecisionKind,
) -> Response {
    match parse::<crate::cluster::membership::Decision>(&body) {
        Some(decision) => peer_json(
            auth.runtime()
                .handle_decision(&node.member_id, decision, kind)
                .await,
        ),
        None => bad_request(),
    }
}

pub(crate) async fn peer_decision(
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(node): Extension<AuthenticatedNode>,
    body: Bytes,
) -> Response {
    peer_decide(auth, node, body, DecisionKind::Commit).await
}

pub(crate) async fn peer_abort(
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(node): Extension<AuthenticatedNode>,
    body: Bytes,
) -> Response {
    peer_decide(auth, node, body, DecisionKind::Abort).await
}

pub(crate) async fn peer_finish(
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(node): Extension<AuthenticatedNode>,
    body: Bytes,
) -> Response {
    match parse::<Finish>(&body) {
        Some(finish) => peer_json(auth.runtime().handle_finish(&node.member_id, finish).await),
        None => bad_request(),
    }
}

pub(crate) async fn peer_status(
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(node): Extension<AuthenticatedNode>,
    body: Bytes,
) -> Response {
    match parse::<StatusRequest>(&body) {
        Some(request) => peer_json(auth.runtime().handle_status(&node.member_id, request).await),
        None => bad_request(),
    }
}

pub(crate) async fn leave_cluster(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
) -> Response {
    if !auth.browser_mutation_allowed(&headers, generation.map(|Extension(g)| g)) {
        return forbidden_origin();
    }
    match auth.runtime().leave_unrecognized().await {
        Ok(()) => match membership_view(auth.runtime(), auth.password_configured()) {
            Ok(view) => reply(
                StatusCode::OK,
                Some(view),
                "本服务器已离开集群，监控已全部关闭",
            ),
            Err(error) => error_reply(&error),
        },
        Err(error) => error_reply(&error),
    }
}

pub(crate) async fn peer_recognition(
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(claim): Extension<crate::cluster::peer_auth::VerifiedRecognition>,
) -> Response {
    let probe = claim.0;
    let cluster_id = probe.cluster_id.clone();
    let member_id = probe.member_id.clone();
    let store = Arc::clone(auth.runtime().membership.store());
    let verdict = tokio::task::spawn_blocking(move || {
        crate::cluster::membership::recognition_verdict(&store, &probe)
    })
    .await;
    let Ok(Ok(verdict)) = verdict else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(identity) = auth.runtime().membership.identity() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    Json(crate::cluster::peer_auth::RecognitionAnswer::sign(
        &cluster_id,
        &member_id,
        verdict,
        &identity,
    ))
    .into_response()
}

pub(crate) async fn peer_forward(
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(node): Extension<AuthenticatedNode>,
    body: Bytes,
) -> Response {
    match parse::<ForwardRequest>(&body) {
        Some(request) => peer_json(
            auth.runtime()
                .handle_forward(&node.member_id, request)
                .await,
        ),
        None => bad_request(),
    }
}

#[cfg(test)]
#[path = "cluster_membership_tests.rs"]
mod tests;
