//! Admin openid binding (design §12, moved into Phase 1).
//!
//! The WebUI opens a short window on the gateway-owning node and shows a
//! pairing code. The first C2C message that is exactly that code (optionally
//! prefixed 「绑定」) binds its `user_openid` as the admin. `any` mode, chosen
//! explicitly, also accepts any message and a FRIEND_ADD. Five wrong messages
//! end the window. Outside a window nothing is bound and the bot never
//! replies.

use serde::Serialize;

pub const DEFAULT_WINDOW_SECS: u64 = 300;
pub const MAX_WINDOW_SECS: u64 = 600;
pub const CODE_LEN: usize = 6;
/// A finished bind (bound or expired) stays visible this long.
const RESULT_VISIBLE_SECS: u64 = 600;
/// Wrong messages allowed in one code window.
pub const MAX_ATTEMPTS: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BindMode {
    /// The message must contain the pairing code (safe default).
    #[default]
    Code,
    /// Any message binds. For a bot only its admins can reach.
    Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindVia {
    Message,
    FriendAdd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindState {
    Idle,
    Waiting {
        code: String,
        mode: BindMode,
        started_at: u64,
        expires_at: u64,
        attempts: u8,
    },
    /// The openid itself is saved and forgotten here: the view, the status
    /// and the SSE event never carry it.
    Bound {
        via: BindVia,
        at: u64,
    },
    Expired {
        at: u64,
    },
}

/// What the gateway handler should do with an inbound event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindDecision {
    /// Save this openid and reply 「绑定成功」 passively.
    Bind { openid: String, via: BindVia },
    /// No window, or the content did not match: log at debug, never reply.
    Ignore,
}

#[derive(Clone, Debug)]
pub struct Binder {
    state: BindState,
}

impl Default for Binder {
    fn default() -> Self {
        Self {
            state: BindState::Idle,
        }
    }
}

/// Random numeric code without a leading zero (so it survives being typed
/// or pasted as a number).
pub fn new_code() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Fall back to the clock; still single-use and short-lived.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bytes = nanos.to_le_bytes();
    }
    let value = u64::from_le_bytes(bytes) % 900_000 + 100_000;
    value.to_string()
}

/// The whole message is the code, optionally prefixed 「绑定」, ignoring
/// spaces and a trailing full stop ("123456", "绑定 123456", " 123456。").
pub fn content_matches(content: &str, code: &str) -> bool {
    if code.is_empty() {
        return false;
    }
    let text = content.trim();
    let text = text.strip_prefix("绑定").unwrap_or(text).trim();
    let text = text
        .strip_suffix('。')
        .or_else(|| text.strip_suffix('.'))
        .unwrap_or(text)
        .trim();
    text == code
}

fn valid_openid(openid: &str) -> bool {
    !openid.is_empty() && openid.len() <= 128 && openid.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl Binder {
    pub fn state(&self) -> &BindState {
        &self.state
    }

    pub fn waiting(&self, now: u64) -> bool {
        matches!(self.state, BindState::Waiting { expires_at, .. } if now < expires_at)
    }

    /// Open (or restart) a window. Returns the code.
    pub fn start(&mut self, now: u64, mode: BindMode, window_secs: u64) -> String {
        let code = new_code();
        self.start_with_code(now, mode, window_secs, code.clone());
        code
    }

    pub fn start_with_code(&mut self, now: u64, mode: BindMode, window_secs: u64, code: String) {
        let window = window_secs.clamp(60, MAX_WINDOW_SECS);
        self.state = BindState::Waiting {
            code,
            mode,
            started_at: now,
            expires_at: now + window,
            attempts: 0,
        };
    }

    pub fn cancel(&mut self) {
        self.state = BindState::Idle;
    }

    /// Expire an overdue window; forget old results.
    pub fn tick(&mut self, now: u64) {
        self.state = match std::mem::replace(&mut self.state, BindState::Idle) {
            BindState::Waiting { expires_at, .. } if now >= expires_at => {
                BindState::Expired { at: expires_at }
            }
            BindState::Bound { at, .. } | BindState::Expired { at }
                if now.saturating_sub(at) > RESULT_VISIBLE_SECS =>
            {
                BindState::Idle
            }
            other => other,
        };
    }

    /// C2C_MESSAGE_CREATE.
    pub fn on_message(&mut self, openid: &str, content: &str, now: u64) -> BindDecision {
        self.tick(now);
        let BindState::Waiting {
            code,
            mode,
            attempts,
            ..
        } = &mut self.state
        else {
            return BindDecision::Ignore;
        };
        if !valid_openid(openid) {
            return BindDecision::Ignore;
        }
        let matched = match mode {
            BindMode::Code => content_matches(content, code),
            BindMode::Any => true,
        };
        if !matched {
            *attempts = attempts.saturating_add(1);
            if *attempts >= MAX_ATTEMPTS {
                // Too many wrong codes: close the window (brute force guard).
                self.state = BindState::Expired { at: now };
            }
            return BindDecision::Ignore;
        }
        self.finish(openid, BindVia::Message, now)
    }

    /// FRIEND_ADD carries no code, so it binds only in an explicitly chosen
    /// `any` window. In code mode the admin still has to send the code.
    pub fn on_friend_add(&mut self, openid: &str, now: u64) -> BindDecision {
        self.tick(now);
        if !matches!(
            self.state,
            BindState::Waiting {
                mode: BindMode::Any,
                ..
            }
        ) || !valid_openid(openid)
        {
            return BindDecision::Ignore;
        }
        self.finish(openid, BindVia::FriendAdd, now)
    }

    fn finish(&mut self, openid: &str, via: BindVia, now: u64) -> BindDecision {
        self.state = BindState::Bound { via, at: now };
        BindDecision::Bind {
            openid: openid.to_string(),
            via,
        }
    }

    /// `/api/notify/status` → `bind`.
    pub fn view(&self, now: u64, node_id: &str) -> serde_json::Value {
        match &self.state {
            BindState::Idle => serde_json::json!({"state": "idle"}),
            BindState::Waiting {
                code,
                mode,
                started_at,
                expires_at,
                attempts,
            } if now < *expires_at => serde_json::json!({
                "state": "waiting",
                "code": code,
                "mode": mode,
                "attempts_left": MAX_ATTEMPTS.saturating_sub(*attempts),
                "started_at": started_at,
                "expires_at": expires_at,
                "remaining_secs": expires_at - now,
                "node_id": node_id,
            }),
            BindState::Waiting { expires_at, .. } => {
                serde_json::json!({"state": "expired", "at": expires_at})
            }
            BindState::Bound { via, at } => serde_json::json!({
                "state": "bound",
                "via": via,
                "at": at,
                "node_id": node_id,
            }),
            BindState::Expired { at } => serde_json::json!({"state": "expired", "at": at}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENID: &str = "E4F0A1B2C3D4E5F60718293A4B5C6D7E";

    fn waiting(binder: &mut Binder, mode: BindMode) {
        binder.start_with_code(1_000, mode, 300, "482913".to_string());
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..50 {
            let code = new_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
            assert_ne!(code.as_bytes()[0], b'0');
        }
    }

    #[test]
    fn matching_needs_the_code_as_a_whole_number() {
        assert!(content_matches("482913", "482913"));
        assert!(content_matches(" 绑定 482913。", "482913"));
        assert!(content_matches("绑定482913", "482913"));
        assert!(!content_matches("我的码 482913", "482913"));
        assert!(!content_matches("482913 482914 482915", "482913"));
        assert!(!content_matches("1482913", "482913"));
        assert!(!content_matches("4829130", "482913"));
        assert!(!content_matches("48291", "482913"));
        assert!(!content_matches("hi", "482913"));
    }

    #[test]
    fn messages_outside_a_window_are_ignored() {
        let mut binder = Binder::default();
        assert_eq!(binder.on_message(OPENID, "hi", 10), BindDecision::Ignore);
        assert_eq!(binder.on_friend_add(OPENID, 10), BindDecision::Ignore);
        assert_eq!(binder.state(), &BindState::Idle);
    }

    #[test]
    fn code_mode_binds_only_on_the_code_and_only_once() {
        let mut binder = Binder::default();
        waiting(&mut binder, BindMode::Code);
        assert_eq!(binder.on_message(OPENID, "hi", 1_010), BindDecision::Ignore);
        assert!(binder.waiting(1_010));
        assert_eq!(
            binder.on_message(OPENID, "482913", 1_020),
            BindDecision::Bind {
                openid: OPENID.into(),
                via: BindVia::Message
            }
        );
        // Window closed: a second message does nothing.
        assert_eq!(
            binder.on_message("OTHER0000000000000000000000000000", "482913", 1_030),
            BindDecision::Ignore
        );
        assert_eq!(binder.view(1_030, "jp")["state"], "bound");
    }

    #[test]
    fn bound_bind_view_has_no_openid() {
        let mut binder = Binder::default();
        waiting(&mut binder, BindMode::Any);
        assert!(matches!(
            binder.on_message(OPENID, "hi", 1_001),
            BindDecision::Bind { .. }
        ));
        let view = binder.view(1_002, "jp");
        assert_eq!(view["state"], "bound");
        assert_eq!(view["via"], "message");
        assert!(view.get("openid").is_none());
        assert!(!view.to_string().contains(OPENID));
        assert!(!format!("{:?}", binder.state()).contains(OPENID));
    }

    #[test]
    fn any_mode_accepts_a_plain_greeting() {
        let mut binder = Binder::default();
        waiting(&mut binder, BindMode::Any);
        assert!(matches!(
            binder.on_message(OPENID, "hi", 1_001),
            BindDecision::Bind { .. }
        ));
    }

    #[test]
    fn friend_add_needs_an_any_window() {
        let mut binder = Binder::default();
        waiting(&mut binder, BindMode::Code);
        assert_eq!(binder.on_friend_add(OPENID, 1_100), BindDecision::Ignore);
        assert!(binder.waiting(1_100));
        waiting(&mut binder, BindMode::Any);
        assert_eq!(
            binder.on_friend_add(OPENID, 1_100),
            BindDecision::Bind {
                openid: OPENID.into(),
                via: BindVia::FriendAdd
            }
        );
    }

    #[test]
    fn five_wrong_codes_close_the_window() {
        let mut binder = Binder::default();
        waiting(&mut binder, BindMode::Code);
        for i in 0..MAX_ATTEMPTS - 1 {
            assert_eq!(
                binder.on_message(OPENID, &format!("{}", 100_000 + i as u32), 1_010),
                BindDecision::Ignore
            );
            assert!(binder.waiting(1_010));
        }
        assert_eq!(binder.view(1_010, "jp")["attempts_left"], 1);
        assert_eq!(
            binder.on_message(OPENID, "000000", 1_011),
            BindDecision::Ignore
        );
        assert_eq!(binder.state(), &BindState::Expired { at: 1_011 });
        assert_eq!(
            binder.on_message(OPENID, "482913", 1_012),
            BindDecision::Ignore
        );
    }

    #[test]
    fn window_expires_and_rejects_bad_openids() {
        let mut binder = Binder::default();
        waiting(&mut binder, BindMode::Any);
        assert_eq!(
            binder.on_message("bad id!", "hi", 1_001),
            BindDecision::Ignore
        );
        let view = binder.view(1_100, "jp");
        assert_eq!(view["state"], "waiting");
        assert_eq!(view["code"], "482913");
        assert_eq!(view["remaining_secs"], 200);
        assert_eq!(binder.on_message(OPENID, "hi", 1_300), BindDecision::Ignore);
        assert_eq!(binder.state(), &BindState::Expired { at: 1_300 });
        assert_eq!(binder.view(1_301, "jp")["state"], "expired");
        binder.tick(1_300 + 601);
        assert_eq!(binder.state(), &BindState::Idle);
    }
}
