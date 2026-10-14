//! Gateway session against a loopback WebSocket server (tests only).

use super::*;
use crate::notify::test_support::MockQq;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// One scripted connection: what the server does after accepting.
#[derive(Clone)]
enum Script {
    /// Hello, wait for identify/resume (recorded), READY, then the given
    /// dispatch frames, then close with the code.
    Session { frames: Vec<Value>, close: u16 },
    /// Accept and say nothing.
    Silent,
    /// Hello, wait for identify (recorded), then close 4004 before READY.
    RejectAuth,
}

struct Server {
    url: String,
    /// The op (2 identify / 6 resume) each connection received.
    ops: Arc<std::sync::Mutex<Vec<i64>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn ws_server(scripts: Vec<Script>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&ops);
    let task = tokio::spawn(async move {
        for script in scripts {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            match script {
                Script::Silent => {
                    // Hold the socket open until the client gives up.
                    while let Some(Ok(_)) = ws.next().await {}
                }
                Script::RejectAuth => {
                    let hello = json!({"op": 10, "d": {"heartbeat_interval": 45000}});
                    ws.send(Message::Text(hello.to_string().into()))
                        .await
                        .unwrap();
                    while let Some(Ok(message)) = ws.next().await {
                        if let Message::Text(text) = message {
                            let value: Value = serde_json::from_str(&text).unwrap();
                            seen.lock().unwrap().push(value["op"].as_i64().unwrap());
                            break;
                        }
                    }
                    let _ = ws
                        .close(Some(CloseFrame {
                            code: CloseCode::from(4004),
                            reason: "".into(),
                        }))
                        .await;
                    while let Some(Ok(_)) = ws.next().await {}
                }
                Script::Session { frames, close } => {
                    let hello = json!({"op": 10, "d": {"heartbeat_interval": 45000}});
                    ws.send(Message::Text(hello.to_string().into()))
                        .await
                        .unwrap();
                    let op = loop {
                        match ws.next().await {
                            Some(Ok(Message::Text(text))) => {
                                let value: Value = serde_json::from_str(&text).unwrap();
                                break value["op"].as_i64().unwrap();
                            }
                            Some(Ok(_)) => continue,
                            _ => return,
                        }
                    };
                    seen.lock().unwrap().push(op);
                    let ready = json!({"op": 0, "s": 1, "t": "READY", "id": "READY:1",
                        "d": {"session_id": "sess-1", "version": 1}});
                    ws.send(Message::Text(ready.to_string().into()))
                        .await
                        .unwrap();
                    for frame in frames {
                        ws.send(Message::Text(frame.to_string().into()))
                            .await
                            .unwrap();
                    }
                    let _ = ws
                        .close(Some(CloseFrame {
                            code: CloseCode::from(close),
                            reason: "".into(),
                        }))
                        .await;
                    while let Some(Ok(_)) = ws.next().await {}
                }
            }
        }
    });
    Server { url, ops, task }
}

fn params(mock: &MockQq, tokens: Arc<TokenCache>) -> GatewayParams {
    GatewayParams {
        http: reqwest::Client::new(),
        tokens,
        api_base: mock.base.clone(),
        token_url: mock.token_url(),
        app_id: "1234567890".into(),
        secret: crate::notify::store::AppSecret::new("synthetic-secret"),
        handshake_timeout: Duration::from_millis(500),
        initial_backoff: Duration::from_millis(20),
        throttle_backoff: Duration::from_millis(200),
    }
}

fn c2c(id: &str, seq: u64) -> Value {
    json!({"op": 0, "s": seq, "t": "C2C_MESSAGE_CREATE", "id": id,
        "d": {"id": format!("msg-{id}"), "content": "hi",
              "author": {"user_openid": "E4F0A1B2C3D4E5F60718293A4B5C6D7E"}}})
}

async fn collect_until_fatal(
    params: GatewayParams,
) -> (Vec<GatewayEvent>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel(64);
    let task = tokio::spawn(run(params, tx));
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        let fatal = matches!(event, GatewayEvent::Closed { fatal: true, .. });
        events.push(event);
        if fatal {
            break;
        }
    }
    (events, task)
}

#[tokio::test]
async fn fatal_close_stops_and_duplicates_are_dropped() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let server = ws_server(vec![Script::Session {
        // The same dispatch id twice (a replay) arrives once.
        frames: vec![c2c("E1", 2), c2c("E1", 2), c2c("E2", 3)],
        close: 4014,
    }])
    .await;
    mock.state.lock().unwrap().gateway_url = Some(server.url.clone());
    let (events, task) = collect_until_fatal(params(&mock, Arc::default())).await;
    let messages = events
        .iter()
        .filter(|event| matches!(event, GatewayEvent::C2c { .. }))
        .count();
    assert_eq!(messages, 2, "{events:?}");
    assert!(matches!(
        events.last(),
        Some(GatewayEvent::Closed { fatal: true, reason, .. }) if reason.contains("4014")
    ));
    // The task ended by itself: no reconnect after a fatal close.
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("gateway task stops")
        .unwrap();
    assert_eq!(*server.ops.lock().unwrap(), vec![2]);
}

#[tokio::test]
async fn invalid_session_identifies_again_and_4004_refetches_the_token() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let server = ws_server(vec![
        // 4009: resume.
        Script::Session {
            frames: vec![],
            close: 4009,
        },
        // Session dead: identify, not resume.
        Script::Session {
            frames: vec![],
            close: 4006,
        },
        // Token rejected: new token, identify.
        Script::Session {
            frames: vec![],
            close: 4004,
        },
        Script::Session {
            frames: vec![],
            close: 4915,
        },
    ])
    .await;
    mock.state.lock().unwrap().gateway_url = Some(server.url.clone());
    let (_events, _task) = collect_until_fatal(params(&mock, Arc::default())).await;
    assert_eq!(*server.ops.lock().unwrap(), vec![2, 6, 2, 2]);
    // One token for the first sessions, one more after 4004.
    assert_eq!(mock.state.lock().unwrap().token_calls, 2);
}

#[tokio::test]
async fn missing_hello_times_out_and_reconnects() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let server = ws_server(vec![
        Script::Silent,
        Script::Session {
            frames: vec![],
            close: 4914,
        },
    ])
    .await;
    mock.state.lock().unwrap().gateway_url = Some(server.url.clone());
    let (events, _task) = collect_until_fatal(params(&mock, Arc::default())).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            GatewayEvent::Closed { fatal: false, reason, .. } if reason.contains("握手超时")
        )),
        "{events:?}"
    );
    assert!(events.contains(&GatewayEvent::Ready));
    assert_eq!(*server.ops.lock().unwrap(), vec![2]);
}

#[tokio::test]
async fn concurrent_token_requests_fetch_once() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let tokens = Arc::new(TokenCache::default());
    let http = reqwest::Client::new();
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let tokens = Arc::clone(&tokens);
        let http = http.clone();
        let url = mock.token_url();
        tasks.push(tokio::spawn(async move {
            tokens
                .get(&http, &url, "1234567890", "synthetic-secret")
                .await
                .map(|token| token.expose().to_string())
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), "synthetic-token");
    }
    assert_eq!(mock.state.lock().unwrap().token_calls, 1);
}

#[tokio::test]
async fn two_4004_closes_before_ready_are_bad_credentials() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let server = ws_server(vec![Script::RejectAuth, Script::RejectAuth, Script::Silent]).await;
    mock.state.lock().unwrap().gateway_url = Some(server.url.clone());
    let (events, task) = collect_until_fatal(params(&mock, Arc::default())).await;
    // The first 4004 retries on a fresh token, the second one is final.
    assert!(
        matches!(
            events.last(),
            Some(GatewayEvent::Closed {
                fatal: true,
                bad_credentials: true,
                ..
            })
        ),
        "{events:?}"
    );
    let first = &events[0];
    assert!(
        matches!(
            first,
            GatewayEvent::Closed {
                fatal: false,
                bad_credentials: false,
                ..
            }
        ),
        "{events:?}"
    );
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("gateway task stops")
        .unwrap();
    assert_eq!(*server.ops.lock().unwrap(), vec![2, 2]);
    assert_eq!(mock.state.lock().unwrap().token_calls, 2);
}

#[tokio::test]
async fn a_4004_after_ready_is_not_counted_toward_bad_credentials() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    let close = |code| Script::Session {
        frames: vec![],
        close: code,
    };
    // READY, 4004, then one pre-READY 4004, then READY and a fatal close:
    // never two pre-READY rejections in a row.
    let server = ws_server(vec![close(4004), Script::RejectAuth, close(4914)]).await;
    mock.state.lock().unwrap().gateway_url = Some(server.url.clone());
    let (events, _task) = collect_until_fatal(params(&mock, Arc::default())).await;
    assert!(
        !events.iter().any(|event| matches!(
            event,
            GatewayEvent::Closed {
                bad_credentials: true,
                ..
            }
        )),
        "{events:?}"
    );
    assert_eq!(*server.ops.lock().unwrap(), vec![2, 2, 2]);
}

#[tokio::test]
async fn bad_credential_token_codes_end_the_gateway_without_a_connect() {
    crate::install_crypto_provider();
    for code in [100016, 100007, 10004] {
        let mock = MockQq::start().await;
        mock.state.lock().unwrap().token_reply = Some((
            200,
            json!({"code": code, "message": "invalid appid or secret"}),
        ));
        let (events, task) = collect_until_fatal(params(&mock, Arc::default())).await;
        assert!(
            matches!(
                events.as_slice(),
                [GatewayEvent::Closed {
                    fatal: true,
                    bad_credentials: true,
                    ..
                }]
            ),
            "{code}: {events:?}"
        );
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("gateway task stops")
            .unwrap();
        assert_eq!(mock.state.lock().unwrap().token_calls, 1, "{code}");
    }
}

#[tokio::test]
async fn throttled_token_fetch_backs_off_and_retries() {
    crate::install_crypto_provider();
    let mock = MockQq::start().await;
    mock.state.lock().unwrap().token_reply =
        Some((200, json!({"code": 100001, "message": "too frequent"})));
    let (tx, mut rx) = mpsc::channel(64);
    let task = tokio::spawn(run(params(&mock, Arc::default()), tx));
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            first,
            GatewayEvent::Closed {
                fatal: false,
                bad_credentials: false,
                ..
            }
        ),
        "{first:?}"
    );
    let started = tokio::time::Instant::now();
    let _second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    // The throttle wait (200 ms in tests) replaces the 20 ms backoff.
    assert!(started.elapsed() >= Duration::from_millis(150));
    task.abort();
}
