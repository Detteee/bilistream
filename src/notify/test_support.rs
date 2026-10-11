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
    /// What `/gateway` returns; defaults to a closed loopback port.
    pub gateway_url: Option<String>,
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
    let (status, body) = state.token_reply.clone().unwrap_or((
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
    let mut state = state.lock().unwrap();
    state.messages.push((openid, body));
    let (status, body) = state
        .replies
        .pop_front()
        .unwrap_or((200, json!({"id": "msg-1", "timestamp": 1})));
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        Json(body),
    )
}

async fn gateway(State(state): State<Arc<Mutex<MockState>>>) -> Json<Value> {
    let url = state
        .lock()
        .unwrap()
        .gateway_url
        .clone()
        .unwrap_or_else(|| "ws://127.0.0.1:9/".to_string());
    Json(json!({ "url": url }))
}
