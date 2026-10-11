use super::*;
use crate::notify::ledger::NotifyLedger;
use crate::notify::sink::{DeliveryContext, PassiveTarget, Rendered, SendOutcome, Sink};
use crate::notify::test_support::MockQq;
use serde_json::json;

const OPENID: &str = "E4F0A1B2C3D4E5F60718293A4B5C6D7E";

fn sink(mock: &MockQq) -> QqSink {
    crate::install_crypto_provider();
    QqSink {
        http: reqwest::Client::new(),
        tokens: Arc::new(TokenCache::default()),
        target: QqTarget {
            app_id: "1234567890".into(),
            secret: AppSecret::new("synthetic-secret"),
            openid: OPENID.into(),
            api_base: mock.base.clone(),
            token_url: mock.token_url(),
        },
    }
}

fn message(critical: bool) -> Rendered {
    Rendered {
        text: "🔴 节点故障 · b".into(),
        minimal: "节点故障 b".into(),
        markdown: None,
        critical,
        undelivered_included: false,
    }
}

#[tokio::test]
async fn active_send_succeeds_and_charges_the_budget() {
    let mock = MockQq::start().await;
    let sink = sink(&mock);
    let mut ledger = NotifyLedger::default();
    let outcome = sink
        .send(
            &message(true),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: None,
                now: 1_000,
            },
        )
        .await;
    assert!(matches!(
        outcome,
        SendOutcome::Delivered { mode: "active", .. }
    ));
    let messages = mock.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, OPENID);
    assert_eq!(messages[0].1["msg_type"], 0);
    assert!(messages[0].1.get("msg_id").is_none());
    assert_eq!(ledger.active_day.values().map(|c| c.count).sum::<u32>(), 1);
}

#[tokio::test]
async fn passive_window_is_used_first() {
    let mock = MockQq::start().await;
    let sink = sink(&mock);
    let mut ledger = NotifyLedger::default();
    let mut passive = PassiveTarget {
        msg_id: "m-1".into(),
        received_at: 990,
        replies_used: 0,
    };
    let outcome = sink
        .send(
            &message(false),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: Some(&mut passive),
                now: 1_000,
            },
        )
        .await;
    assert!(matches!(
        outcome,
        SendOutcome::Delivered {
            mode: "passive",
            ..
        }
    ));
    assert_eq!(mock.messages()[0].1["msg_id"], "m-1");
    assert_eq!(mock.messages()[0].1["msg_seq"], 1);
    assert_eq!(passive.replies_used, 1);
    assert_eq!(ledger.active_day.values().map(|c| c.count).sum::<u32>(), 0);
}

#[tokio::test]
async fn active_unavailable_falls_through_to_one_wakeup_for_critical() {
    let mock = MockQq::start().await;
    mock.script([(400, json!({"code": 40034105, "message": "no active"}))]);
    let sink = sink(&mock);
    let mut ledger = NotifyLedger::default();
    ledger.record_admin_message(900);
    let outcome = sink
        .send(
            &message(true),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: None,
                now: 1_000,
            },
        )
        .await;
    assert!(
        matches!(outcome, SendOutcome::Delivered { mode: "wakeup", .. }),
        "{outcome:?}"
    );
    assert_eq!(mock.messages()[1].1["is_wakeup"], true);
    // The recall carries the 「please reply」 line with the next recall date.
    let content = mock.messages()[1].1["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(content.contains("召回提醒还剩 3 次（下次 "), "{content}");
    assert!(!mock.messages()[0].1["content"]
        .as_str()
        .unwrap()
        .contains("召回"));
    assert!(ledger.modes.active_unavailable(1_000));
    assert!(!ledger.wakeup_available(1_001));
}

#[tokio::test]
async fn warnings_never_spend_a_wakeup() {
    let mock = MockQq::start().await;
    mock.script([(400, json!({"code": 40034105}))]);
    let sink = sink(&mock);
    let mut ledger = NotifyLedger::default();
    ledger.record_admin_message(900);
    let outcome = sink
        .send(
            &message(false),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: None,
                now: 1_000,
            },
        )
        .await;
    assert!(matches!(
        outcome,
        SendOutcome::Undeliverable {
            bad_credentials: false,
            ..
        }
    ));
    assert_eq!(mock.messages().len(), 1);
    assert!(ledger.wakeup_available(1_001));
}

#[tokio::test]
async fn rate_limit_and_server_errors_retry_later() {
    let mock = MockQq::start().await;
    mock.script([(429, json!({"code": 40034100}))]);
    let sink = sink(&mock);
    let mut ledger = NotifyLedger::default();
    let outcome = sink
        .send(
            &message(true),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: None,
                now: 1_000,
            },
        )
        .await;
    assert!(matches!(outcome, SendOutcome::Retry { .. }));
    assert!(ledger.rate_limited_until >= 1_060);
    mock.script([(502, json!({}))]);
    let outcome = sink
        .send(
            &message(true),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: None,
                now: 2_000,
            },
        )
        .await;
    assert!(matches!(outcome, SendOutcome::Retry { .. }));
}

#[tokio::test]
async fn expired_token_is_refetched_once() {
    let mock = MockQq::start().await;
    mock.script([(401, json!({"code": 11244}))]);
    let sink = sink(&mock);
    let mut ledger = NotifyLedger::default();
    let outcome = sink
        .send(
            &message(true),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: None,
                now: 1_000,
            },
        )
        .await;
    assert!(
        matches!(outcome, SendOutcome::Delivered { .. }),
        "{outcome:?}"
    );
    assert_eq!(mock.state.lock().unwrap().token_calls, 2);
}

#[tokio::test]
async fn bad_credentials_are_undeliverable_and_flagged() {
    let mock = MockQq::start().await;
    mock.state.lock().unwrap().token_reply =
        Some((200, json!({"code": 100016, "message": "invalid appid"})));
    let sink = sink(&mock);
    let mut ledger = NotifyLedger::default();
    let outcome = sink
        .send(
            &message(true),
            &mut DeliveryContext {
                ledger: &mut ledger,
                passive: None,
                now: 1_000,
            },
        )
        .await;
    assert!(
        matches!(
            outcome,
            SendOutcome::Undeliverable {
                bad_credentials: true,
                code: Some(100016),
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(mock.messages().is_empty());
}

#[tokio::test]
async fn token_is_cached_and_reused() {
    let mock = MockQq::start().await;
    let sink = sink(&mock);
    for _ in 0..3 {
        sink.token().await.unwrap();
    }
    assert_eq!(mock.state.lock().unwrap().token_calls, 1);
    assert_eq!(format!("{:?}", sink.token().await.unwrap()), "[redacted]");
}

#[test]
fn bodies_match_the_documented_shapes() {
    let passive = api::c2c_body(&api::C2cMessage {
        openid: OPENID,
        content: "绑定成功",
        markdown: false,
        mode: QqMode::Passive,
        msg_id: None,
        event_id: Some("FRIEND_ADD:1"),
        msg_seq: 1,
    });
    assert_eq!(
        passive,
        json!({"msg_type": 0, "content": "绑定成功", "event_id": "FRIEND_ADD:1", "msg_seq": 1})
    );
    let markdown = api::c2c_body(&api::C2cMessage {
        openid: OPENID,
        content: "**x**",
        markdown: true,
        mode: QqMode::Wakeup,
        msg_id: None,
        event_id: None,
        msg_seq: 0,
    });
    assert_eq!(
        markdown,
        json!({"msg_type": 2, "markdown": {"content": "**x**"}, "is_wakeup": true})
    );
}
