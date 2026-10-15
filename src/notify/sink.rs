//! Delivery sink interface (design §5). QQ is the only sink; the trait keeps
//! the door open for another channel later and lets tests use a fake.

use super::ledger::NotifyLedger;

/// Identity of one logical message. It travels separately from the rendered
/// text and credentials; each actual C2C request (including retries) records
/// this context once at the transport boundary.
#[derive(Clone, Debug)]
pub struct AttemptContext {
    pub key: String,
    pub dedup: String,
    pub phase: &'static str,
    pub test: bool,
    pub suppressed: u32,
    pub undelivered: u32,
}

impl AttemptContext {
    pub fn new(key: impl Into<String>, dedup: impl Into<String>, phase: &'static str) -> Self {
        Self {
            key: key.into(),
            dedup: dedup.into(),
            phase,
            test: false,
            suppressed: 0,
            undelivered: 0,
        }
    }

    pub(crate) fn event(
        &self,
        mode: &'static str,
        ok: bool,
        code: Option<i64>,
        trace_id: Option<String>,
    ) -> crate::events::EventKind {
        crate::events::EventKind::NotifySent {
            key: crate::logging::safe_text(&self.key, 120),
            dedup: crate::logging::safe_text(&self.dedup, 512),
            phase: self.phase,
            mode: Some(mode),
            ok,
            code,
            trace_id,
            test: self.test,
            suppressed: self.suppressed,
            undelivered: self.undelivered,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rendered {
    pub attempt: AttemptContext,
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
