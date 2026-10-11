//! Delivery sink interface (design §5). QQ is the only sink; the trait keeps
//! the door open for another channel later and lets tests use a fake.

use super::ledger::NotifyLedger;

#[derive(Clone, Debug)]
pub struct Rendered {
    pub text: String,
    /// Fallback body when QQ rejects the content.
    pub minimal: String,
    /// Markdown body, when the style asks for it.
    pub markdown: Option<String>,
    /// Critical alerts may spend an `is_wakeup` recall.
    pub critical: bool,
    /// Every body carries the 「另有 N 条通知未送达」 count; on delivery the
    /// count is marked reported.
    pub undelivered_included: bool,
}

/// The admin's latest message seen by this node's gateway session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassiveTarget {
    pub msg_id: String,
    pub received_at: u64,
    pub replies_used: u32,
}

/// QQ allows 4 passive replies per message within its reply window.
pub const PASSIVE_MAX_REPLIES: u32 = 4;
pub const PASSIVE_WINDOW_SECS: u64 = 3600;

impl PassiveTarget {
    pub fn usable(&self, now: u64) -> bool {
        !self.msg_id.is_empty()
            && self.replies_used < PASSIVE_MAX_REPLIES
            && now.saturating_sub(self.received_at) < PASSIVE_WINDOW_SECS
    }
}

pub struct DeliveryContext<'a> {
    pub ledger: &'a mut NotifyLedger,
    pub passive: Option<&'a mut PassiveTarget>,
    pub now: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered {
        mode: &'static str,
        trace_id: Option<String>,
    },
    /// Try again later (network, 5xx, rate limit, own budget spent).
    Retry {
        reason: String,
        code: Option<i64>,
        trace_id: Option<String>,
    },
    /// No mode can deliver it. Logged once, counted, never rerouted.
    Undeliverable {
        reason: String,
        code: Option<i64>,
        trace_id: Option<String>,
        /// AppID/AppSecret rejected: stop all QQ calls until settings change.
        bad_credentials: bool,
    },
}

// async_trait marks the boxed future #[must_use] on its own.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    async fn send(&self, message: &Rendered, ctx: &mut DeliveryContext<'_>) -> SendOutcome;
}
