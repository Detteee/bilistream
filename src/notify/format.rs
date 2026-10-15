//! Message rendering (design §10, alert format spec
//! `/workspace/qq-notify-alert-formats.md`).
//!
//! - First line states what happened; the footer identifies the sender.
//! - Plain text by default, at most 500 characters. When too long, lines
//!   are dropped in a fixed order; the first line, delayed-event time,
//!   sender and 未送达 count are retained.
//! - Every interpolated value is cleaned: no URLs (`a[.]b`), `<` `>` made
//!   full-width, Markdown specials made full-width in Markdown bodies, and
//!   cut by display width (node names to 24 columns).

use super::event::{reason_text, EventKind, Severity};
use super::ledger::NotifyLedger;
use super::policy::{ItemKind, OutItem, LATCHED_REASONS};
use chrono::{FixedOffset, TimeZone};
use std::collections::HashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const MAX_CHARS: usize = 500;
/// Summary entries shown before 「另有 N 条」.
const MAX_LINES: usize = 5;
/// Summary entries kept when the message is too long (drop stage 3).
const MIN_LINES: usize = 3;
pub const NODE_COLS: usize = 24;
const DETAIL_COLS: usize = 160;
const SUMMARY_REASON_COLS: usize = 60;
const ERROR_HEAD_COLS: usize = 100;
const ERROR_TAIL_COLS: usize = 40;
pub const LABEL_FALLBACK: &str = "（无多数派）";
/// Beyond ordinary confirmation/coalescing, show when the queued fact happened.
const DELAYED_NOTICE_SECS: u64 = 300;

#[derive(Clone, Debug, Default)]
pub struct RenderContext {
    pub now: u64,
    /// Node id of the sending node.
    pub reporter: String,
    pub active_owner: Option<String>,
    /// `（无多数派）`, when this sender only observes its partition.
    pub label: Option<String>,
    pub undelivered: u32,
    pub undelivered_reason: Option<String>,
    pub markdown: bool,
    /// Configured node names by node id (`ClusterNodeSnapshot.name`).
    pub names: HashMap<String, String>,
    /// Resolved items that could not be delivered earlier; attached as
    /// `· ✅ … 已恢复（…，HH:mm 恢复）` lines.
    pub held: Vec<OutItem>,
}

impl RenderContext {
    /// Configured name, or the id when the name is empty or equal to it.
    pub fn node_name(&self, id: &str) -> String {
        let name = self
            .names
            .get(id)
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or(id);
        self.value(name, NODE_COLS)
    }

    /// Clean one interpolated value for this body.
    pub fn value(&self, raw: &str, cols: usize) -> String {
        let text = sanitize(raw, cols);
        if self.markdown {
            markdown_safe(&text)
        } else {
            text
        }
    }
}

pub fn utc8_time(at: u64) -> String {
    utc8(at, "%Y-%m-%d %H:%M")
}

fn utc8(at: u64, pattern: &str) -> String {
    let offset = FixedOffset::east_opt(8 * 3600).expect("UTC+8 offset");
    offset
        .timestamp_opt(at as i64, 0)
        .single()
        .map(|time| time.format(pattern).to_string())
        .unwrap_or_default()
}

/// Other times in a message: `HH:mm` on the same day, `MM-DD HH:mm`
/// otherwise (spec §10.1).
pub fn short_time(at: u64, now: u64) -> String {
    if utc8(at, "%Y-%m-%d") == utc8(now, "%Y-%m-%d") {
        utc8(at, "%H:%M")
    } else if utc8(at, "%Y") == utc8(now, "%Y") {
        utc8(at, "%m-%d %H:%M")
    } else {
        utc8(at, "%Y-%m-%d %H:%M")
    }
}

pub fn month_day(at: u64) -> String {
    utc8(at, "%m-%d")
}

pub fn duration_text(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分钟", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时 {} 分钟", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{} 天 {} 小时", secs / 86_400, (secs % 86_400) / 3600)
    }
}

// ---------------------------------------------------------------------------
// Cleaning

/// Strip `scheme://` and turn every domain-like token or IPv4 address into
/// `a[.]b`, so QQ never sees a link (304003).
pub fn delink(raw: &str) -> String {
    let mut text = raw.to_string();
    while let Some(pos) = text.find("://") {
        let start = text[..pos]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map(|i| i + 1)
            .unwrap_or(0);
        text.replace_range(start..pos + 3, "");
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if looks_like_host(token) {
            out.push_str(&token.replace('.', "[.]"));
        } else {
            out.push_str(token);
        }
        token.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            token.push(ch);
        } else {
            flush(&mut token, &mut out);
            out.push(ch);
        }
    }
    flush(&mut token, &mut out);
    out
}

fn looks_like_host(token: &str) -> bool {
    let token = token.trim_end_matches('.');
    let labels: Vec<&str> = token.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|label| label.is_empty()) {
        return false;
    }
    let ipv4 = labels.len() == 4
        && labels
            .iter()
            .all(|label| label.len() <= 3 && label.bytes().all(|b| b.is_ascii_digit()));
    let last = labels.last().copied().unwrap_or_default();
    let domain = (2..=24).contains(&last.len()) && last.bytes().all(|b| b.is_ascii_alphabetic());
    ipv4 || domain
}

/// Keep QQ rich-text tags out of interpolated values.
fn fullwidth_angles(text: &str) -> String {
    text.replace('<', "＜").replace('>', "＞")
}

fn markdown_safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '*' => '＊',
            '_' => '＿',
            '~' => '～',
            '`' => '｀',
            '[' => '［',
            ']' => '］',
            '(' => '（',
            ')' => '）',
            '#' => '＃',
            '|' => '｜',
            '\\' => '＼',
            '!' => '！',
            other => other,
        })
        .collect()
}

/// Redact before delinking, so removing a URL scheme cannot hide a secret
/// from the shared sanitizer. Then neutralise tags and fit the text budget.
pub fn sanitize(raw: &str, cols: usize) -> String {
    let redacted = crate::logging::safe_text(raw, usize::MAX);
    let cleaned = fullwidth_angles(&delink(&redacted));
    // Combining/zero-width characters must not evade QQ's character limit
    // even when their display width fits in a node-name column budget.
    truncate_chars(&crate::logging::safe_text(&cleaned, cols), cols)
}

/// Cut a long error in the middle: the first `head` and last `tail` columns
/// survive, because exit codes and error codes sit at the end.
pub fn shorten_middle(text: &str, head: usize, tail: usize) -> String {
    if UnicodeWidthStr::width(text) <= head + tail + 1 {
        return text.to_string();
    }
    let mut start = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > head {
            break;
        }
        width += w;
        start.push(ch);
    }
    let mut end: Vec<char> = Vec::new();
    width = 0;
    for ch in text.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > tail {
            break;
        }
        width += w;
        end.push(ch);
    }
    end.reverse();
    format!("{start}…{}", end.into_iter().collect::<String>())
}

/// A raw one-line error for `detail`: whitespace collapsed, absolute paths
/// reduced to the file name, then cut in the middle (100 + 40 columns).
pub fn clean_detail(raw: &str) -> String {
    let collapsed: Vec<String> = raw
        .split_whitespace()
        .map(|word| {
            if word.starts_with('/') && word.matches('/').count() > 1 {
                word.rsplit('/').next().unwrap_or(word).to_string()
            } else {
                word.to_string()
            }
        })
        .collect();
    let one_line = crate::logging::safe_text(&collapsed.join(" "), 2000);
    shorten_middle(&one_line, ERROR_HEAD_COLS, ERROR_TAIL_COLS)
}

// ---------------------------------------------------------------------------
// Pieces

fn variant(item: &OutItem) -> &str {
    item.key.splitn(3, ':').nth(2).unwrap_or_default()
}

fn marker(item: &OutItem) -> &'static str {
    match (&item.item, item.kind) {
        (ItemKind::Resolved { .. }, _) => "✅",
        (_, EventKind::StreamLive) => "🟢",
        (_, EventKind::Test) => "🧪",
        _ => match item.severity() {
            Severity::Critical => "🔴",
            Severity::Warning => "🟠",
            Severity::Info => "🔵",
        },
    }
}

fn platform_name(platform: &str) -> &str {
    match platform {
        "youtube" => "YouTube",
        "twitch" => "Twitch",
        "niconico" => "Niconico",
        other => other,
    }
}

fn reason(item: &OutItem, ctx: &RenderContext, cols: usize) -> Option<String> {
    let raw = item.detail.clone().or_else(|| {
        let variant = variant(item);
        (!variant.is_empty()).then(|| reason_text(variant))
    })?;
    Some(ctx.value(&raw, cols))
}

/// `A → B` for a failover, or the node / channel name.
fn subject(item: &OutItem, ctx: &RenderContext) -> String {
    if let Some(live) = &item.extra.live {
        return ctx.value(&live.name, NODE_COLS);
    }
    if item.kind == EventKind::Failover {
        if let Some(old) = &item.extra.old_owner {
            return format!("{} → {}", ctx.node_name(old), ctx.node_name(&item.node));
        }
    }
    ctx.node_name(&item.node)
}

fn headline(item: &OutItem, ctx: &RenderContext) -> String {
    let node = subject(item, ctx);
    if matches!(item.item, ItemKind::Resolved { .. }) {
        return match item.kind {
            EventKind::QuorumLost => format!("{node} 已恢复多数派连接"),
            EventKind::SelfCheckFailing => format!("{node} 的自检已恢复正常"),
            EventKind::NodeDown => format!("{node} 已恢复连接"),
            _ => format!("{node} 的状态已恢复正常"),
        };
    }
    if item.flapping.is_some() {
        return format!("{node} 的状态反复变化");
    }
    match item.kind {
        EventKind::NodeDown => format!("{node} 已失去连接"),
        EventKind::NodeDegraded if LATCHED_REASONS.contains(&variant(item)) => {
            format!("{node} 已暂停转播")
        }
        EventKind::NodeDegraded => format!("{node} 运行异常"),
        EventKind::SelfCheckFailing => format!("{node} 自检未通过"),
        EventKind::QuorumLost => format!("{node} 无法连接多数节点"),
        EventKind::Failover => match item.extra.old_owner.as_deref() {
            Some(old) => format!(
                "转播节点已从 {} 切换到 {}",
                ctx.node_name(old),
                ctx.node_name(&item.node)
            ),
            None => format!("{} 已成为转播节点", ctx.node_name(&item.node)),
        },
        EventKind::StreamLive => format!("{node} 开播了"),
        EventKind::Test => "通知测试".into(),
    }
}

fn reason_sentence(
    item: &OutItem,
    ctx: &RenderContext,
    cols: usize,
    include_state: bool,
) -> Option<String> {
    if item.kind == EventKind::QuorumLost {
        let detail = reason(item, ctx, cols)
            .filter(|detail| detail != "本节点失去多数派")
            .map(|detail| format!("{}。", detail.trim_end_matches('。')))
            .unwrap_or_default();
        return Some(format!("{detail}目前无法确认其他节点的状态。"));
    }
    let Some(detail) = reason(item, ctx, cols) else {
        return include_state.then(|| format!("{}。", headline(item, ctx)));
    };
    let detail = detail.trim_end_matches(['。', '.', '；']);
    let node = ctx.node_name(&item.node);
    let pause = item.kind == EventKind::NodeDegraded && LATCHED_REASONS.contains(&variant(item));
    Some(if include_state && pause {
        format!("{node} 因{detail}暂停了转播。")
    } else if include_state {
        format!("{}（{detail}）。", headline(item, ctx))
    } else if item.kind == EventKind::Failover {
        let old = item.extra.old_owner.as_deref().map(|id| ctx.node_name(id));
        match old {
            Some(old) => format!("{old} {detail}。"),
            None => format!("{detail}。"),
        }
    } else {
        format!("{detail}。")
    })
}

fn occurred_at(item: &OutItem) -> u64 {
    match item.item {
        ItemKind::Resolved { duration_secs } => item.incident_at.saturating_add(duration_secs),
        ItemKind::Alert => item.incident_at,
    }
}

fn held_line(item: &OutItem, ctx: &RenderContext) -> String {
    let duration = match item.item {
        ItemKind::Resolved { duration_secs } => duration_text(duration_secs),
        ItemKind::Alert => String::new(),
    };
    format!(
        "· ✅ {}（异常持续 {}，{} 恢复）",
        headline(item, ctx),
        duration,
        short_time(occurred_at(item), ctx.now)
    )
}

fn summary_line(item: &OutItem, ctx: &RenderContext) -> String {
    match &item.item {
        ItemKind::Resolved { duration_secs } => format!(
            "· ✅ {}，异常持续 {}。",
            headline(item, ctx),
            duration_text(*duration_secs)
        ),
        ItemKind::Alert if item.kind == EventKind::StreamLive => {
            let platform = item
                .extra
                .live
                .as_ref()
                .map(|live| {
                    live.platforms
                        .iter()
                        .map(|p| platform_name(p))
                        .collect::<Vec<_>>()
                        .join("、")
                })
                .unwrap_or_default();
            format!("· 🟢 {} 开播了（{}）", subject(item, ctx), platform)
        }
        ItemKind::Alert => {
            let mut line = format!("· {} {}", marker(item), headline(item, ctx));
            if item.flapping.is_some() {
                line.push_str(&format!(
                    "（{} 分钟内出现 {} 次）",
                    super::policy::FLAP_WINDOW_SECS / 60,
                    item.flapping.unwrap_or_default()
                ));
            } else if let Some(reason) = reason(item, ctx, SUMMARY_REASON_COLS) {
                line.push_str(&format!("：{reason}"));
            }
            line
        }
    }
}

/// The parts of one message, assembled under the length budget.
struct Parts {
    first: String,
    time: String,
    body: Vec<String>,
    /// Summary entries (may be cut from 5 to 3).
    items: Vec<String>,
    sender: String,
    undelivered: Option<String>,
    markdown: bool,
}

impl Parts {
    fn assemble(&self, body: &[String], max_items: usize) -> String {
        let mut lines = vec![self.first.clone()];
        if !self.time.is_empty() {
            lines.push(self.time.clone());
        }
        lines.extend(body.iter().cloned());
        lines.extend(self.items.iter().take(max_items).cloned());
        if self.items.len() > max_items {
            lines.push(format!("另有 {} 条", self.items.len() - max_items));
        }
        if let Some(line) = &self.undelivered {
            lines.push(line.clone());
        }
        lines.push(self.sender.clone());
        decorate(lines, self.markdown)
    }

    /// Drop order (spec §10.2): CTA (appended later, only if it fits),
    /// the raw text in brackets, summary entries 5 → 3, then a hard cut of
    /// the middle lines. The sender and undelivered count always survive.
    fn fit(&self) -> String {
        let full = self.assemble(&self.body, MAX_LINES);
        if full.chars().count() <= MAX_CHARS {
            return full;
        }
        let stripped: Vec<String> = self.body.iter().map(|line| strip_brackets(line)).collect();
        let text = self.assemble(&stripped, MAX_LINES);
        if text.chars().count() <= MAX_CHARS {
            return text;
        }
        let text = self.assemble(&stripped, MIN_LINES);
        if text.chars().count() <= MAX_CHARS {
            return text;
        }
        // Hard cut, but only of the middle: the first line, the time and the
        // 未送达 line always survive.
        let fixed = self.assemble(&[], 0);
        let fixed_len = fixed.chars().count();
        let mut middle: Vec<String> = stripped.clone();
        middle.extend(self.items.iter().take(MIN_LINES).cloned());
        let middle = decorate_tail(middle, self.markdown).join("\n");
        let budget = MAX_CHARS.saturating_sub(fixed_len + 2);
        let cut: String = middle.chars().take(budget.saturating_sub(1)).collect();
        let mut lines: Vec<&str> = fixed.lines().collect();
        let cut_line = format!("{cut}…");
        if budget > 1 {
            lines.insert(if self.time.is_empty() { 1 } else { 2 }, &cut_line);
        }
        let text = lines.join("\n");
        truncate_chars(&text, MAX_CHARS)
    }
}

fn strip_brackets(line: &str) -> String {
    match (line.rfind('（'), line.ends_with('）')) {
        (Some(start), true) if start > 0 => line[..start].to_string(),
        _ => line.to_string(),
    }
}

fn decorate(mut lines: Vec<String>, markdown: bool) -> String {
    if markdown {
        if let Some(first) = lines.first_mut() {
            *first = format!("**{first}**");
        }
        for line in lines.iter_mut().skip(1) {
            *line = format!("- {line}");
        }
    }
    lines.join("\n")
}

fn decorate_tail(lines: Vec<String>, markdown: bool) -> Vec<String> {
    if markdown {
        lines.into_iter().map(|line| format!("- {line}")).collect()
    } else {
        lines
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Render

/// One message for a coalesced batch (already sorted worst first).
pub fn render(batch: &[OutItem], ctx: &RenderContext) -> String {
    let label = if ctx
        .label
        .as_deref()
        .is_some_and(|label| label.contains("无多数派"))
    {
        LABEL_FALLBACK
    } else {
        ""
    };
    let reporter = ctx.node_name(&ctx.reporter);
    let owner = ctx
        .active_owner
        .as_deref()
        .map(|owner| ctx.node_name(owner));
    let undelivered = (ctx.undelivered > 0).then(|| {
        let reason = ctx
            .undelivered_reason
            .as_deref()
            .map(|reason| format!("（{}）", ctx.value(reason, 60)))
            .unwrap_or_default();
        format!("另有 {} 条通知未送达{}", ctx.undelivered, reason)
    });
    // QQ already timestamps delivery. Preserve the event time only when a
    // queued message waited long enough for delivery time to be misleading.
    let earliest = batch.iter().map(occurred_at).min();
    let combined =
        earliest.is_some() && batch.iter().any(|item| Some(occurred_at(item)) != earliest);
    let delayed = batch
        .iter()
        .any(|item| ctx.now.saturating_sub(item.queued_at) >= DELAYED_NOTICE_SECS);
    let time = earliest
        .filter(|_| combined || delayed)
        .map(|at| {
            let what = if batch
                .iter()
                .all(|item| matches!(item.item, ItemKind::Resolved { .. }))
            {
                if combined {
                    "最早恢复于"
                } else {
                    "恢复于"
                }
            } else if combined {
                "最早发生于"
            } else {
                "发生于"
            };
            format!("{what} {} (UTC+8)", utc8_time(at))
        })
        .unwrap_or_default();
    let held: Vec<String> = ctx.held.iter().map(|item| held_line(item, ctx)).collect();
    let mut parts = Parts {
        first: String::new(),
        time,
        body: Vec::new(),
        items: Vec::new(),
        sender: format!("发送节点：{reporter}"),
        undelivered,
        markdown: ctx.markdown,
    };
    match batch {
        [] if held.is_empty() => return String::new(),
        [] => {
            parts.first = format!("{label}✅ {} 项异常已恢复", held.len());
            parts.items = held;
        }
        [item] => {
            single(item, ctx, label, owner.as_deref(), &mut parts);
            parts.items = held;
        }
        items => {
            summary(items, ctx, label, &mut parts);
            parts.items.extend(held);
        }
    }
    parts.fit()
}

fn single(
    item: &OutItem,
    ctx: &RenderContext,
    label: &str,
    owner: Option<&str>,
    parts: &mut Parts,
) {
    parts.first = format!("{label}{} {}", marker(item), headline(item, ctx));
    match (&item.item, item.kind) {
        (ItemKind::Resolved { duration_secs }, _) => {
            let mut extra = format!("本次异常持续 {}", duration_text(*duration_secs));
            if item.suppressed > 0 {
                extra.push_str(&format!("，期间重复 {} 次", item.suppressed));
            }
            if let Some(owner) =
                owner.filter(|_| ctx.active_owner.as_deref() != Some(item.node.as_str()))
            {
                extra.push_str(&format!("，当前转播节点仍为 {owner}"));
            }
            parts.body.push(format!("{extra}。"));
        }
        (_, EventKind::Test) => {
            parts.body = vec!["这是一条 bilistream 测试消息，收到即说明 QQ 通知可用。".to_string()];
        }
        (_, EventKind::StreamLive) => {
            if let Some(live) = &item.extra.live {
                let platform = live
                    .platforms
                    .iter()
                    .map(|p| platform_name(p))
                    .collect::<Vec<_>>()
                    .join("、");
                if let Some(stream_title) = live.title.as_deref().filter(|t| !t.trim().is_empty()) {
                    parts.body.push(format!(
                        "{platform} · {}",
                        ctx.value(stream_title, DETAIL_COLS)
                    ));
                } else if !platform.is_empty() {
                    parts.body.push(platform);
                }
            }
        }
        (_, EventKind::Failover) => {
            if let Some(reason) = reason_sentence(item, ctx, DETAIL_COLS, false) {
                parts.body.push(reason);
            }
        }
        _ if item.flapping.is_some() => {
            parts.body.push(format!(
                "{} 分钟内已出现 {} 次异常。",
                super::policy::FLAP_WINDOW_SECS / 60,
                item.flapping.unwrap_or_default()
            ));
        }
        _ => {
            if let Some(reason) = reason_sentence(item, ctx, DETAIL_COLS, false) {
                let lasted = ctx.now.saturating_sub(item.incident_at);
                if lasted >= 30 {
                    parts
                        .body
                        .push(format!("{reason}异常已持续 {}。", duration_text(lasted)));
                } else {
                    parts.body.push(reason);
                }
            }
            if item.suppressed > 0 {
                parts
                    .body
                    .push(format!("期间重复 {} 次。", item.suppressed));
            }
        }
    }
}

fn summary(items: &[OutItem], ctx: &RenderContext, label: &str, parts: &mut Parts) {
    let alerts: Vec<&OutItem> = items
        .iter()
        .filter(|item| item.item == ItemKind::Alert)
        .collect();
    let all_resolved = alerts.is_empty();
    let all_live =
        !alerts.is_empty() && alerts.iter().all(|item| item.kind == EventKind::StreamLive);
    let top = alerts.iter().map(|item| item.severity()).max();
    let head_marker = if all_resolved {
        "✅"
    } else if all_live {
        "🟢"
    } else {
        match top {
            Some(Severity::Critical) => "🔴",
            Some(Severity::Warning) => "🟠",
            _ => "🔵",
        }
    };
    let failover = alerts
        .iter()
        .find(|item| item.kind == EventKind::Failover && item.extra.old_owner.is_some());
    let first_node = alerts.first().map(|item| item.node.as_str());
    let same_node = !all_live
        && first_node.is_some()
        && alerts
            .iter()
            .all(|item| Some(item.node.as_str()) == first_node)
        && alerts.len() >= 2;
    let incident_title = if let Some(item) = failover {
        headline(item, ctx)
    } else if all_resolved {
        format!("{} 项异常已恢复", items.len())
    } else if all_live {
        format!("{} 个关注频道开播了", alerts.len())
    } else if same_node {
        format!(
            "{} 有 {} 项异常",
            ctx.node_name(&alerts[0].node),
            alerts.len()
        )
    } else {
        format!("集群异常（{} 条）", items.len())
    };
    parts.first = format!("{label}{head_marker} {incident_title}");
    let related = failover.and_then(|failover| {
        items.iter().find(|item| {
            item.item == ItemKind::Alert
                && matches!(item.kind, EventKind::NodeDown | EventKind::NodeDegraded)
                && failover.extra.old_owner.as_deref() == Some(item.node.as_str())
                && reason(item, ctx, DETAIL_COLS) == reason(failover, ctx, DETAIL_COLS)
        })
    });
    if let Some(item) = related.or(failover.copied()) {
        if let Some(reason) = reason_sentence(item, ctx, DETAIL_COLS, related.is_some()) {
            parts.body.push(reason);
        }
    }
    parts.items = items
        .iter()
        .filter(|item| {
            !failover.is_some_and(|head| std::ptr::eq(*item, *head))
                && !related.is_some_and(|detail| std::ptr::eq(*item, detail))
        })
        .map(|item| summary_line(item, ctx))
        .collect();
}

/// Last-resort body when QQ rejects the content (design §7 ContentRejected).
/// It still carries the 未送达 count, so a delivery always reports it.
pub fn minimal(batch: &[OutItem], ctx: &RenderContext) -> String {
    let critical = batch
        .iter()
        .filter(|item| item.severity() == Severity::Critical && item.item == ItemKind::Alert)
        .count();
    let mut text = format!(
        "bilistream 通知 {} 条（严重 {} 条），请查看面板",
        batch.len().max(1),
        critical
    );
    if batch
        .iter()
        .any(|item| item.kind == EventKind::QuorumLost && item.item == ItemKind::Alert)
    {
        text.push_str("（无法连接多数节点）");
    }
    if ctx.undelivered > 0 {
        text.push_str(&format!("；另有 {} 条通知未送达", ctx.undelivered));
    }
    with_sender(&text, &ctx.node_name(&ctx.reporter))
}

/// Plain notices and command replies reserve space for the current sender.
/// Called at rendering time, so a pending notice never keeps a former sender.
pub fn with_sender(body: &str, sender: &str) -> String {
    let footer = format!("发送节点：{}", sanitize(sender, NODE_COLS));
    let body = truncate_chars(body, MAX_CHARS.saturating_sub(footer.chars().count() + 1));
    format!("{body}\n{footer}")
}

// ---------------------------------------------------------------------------
// Call-to-action lines (spec §6). Appended by the sink once it knows the
// mode, and only when they fit (they are the first thing dropped).

pub fn wakeup_cta(ledger: &NotifyLedger, now: u64) -> String {
    match ledger.wakeup_after_use(now) {
        Some((left, next)) if left > 0 => {
            let next = next
                .map(|at| format!("（下次 {} 起可用）", month_day(at)))
                .unwrap_or_default();
            format!("召回消息还剩 {left} 次{next}。回复任意消息即可恢复正常推送。")
        }
        Some(_) => "这是本周期最后一次召回消息。回复任意消息即可恢复正常推送。".to_string(),
        None => "这是一次召回消息。回复任意消息即可恢复正常推送。".to_string(),
    }
}

pub fn last_reply_cta() -> String {
    "本轮可回复次数已用完。回复任意消息即可恢复正常推送。".to_string()
}

pub fn with_cta(body: &str, cta: Option<&str>, markdown: bool) -> String {
    let Some(cta) = cta else {
        return body.to_string();
    };
    let line = if markdown {
        format!("- {cta}")
    } else {
        cta.to_string()
    };
    if body.chars().count() + 1 + line.chars().count() <= MAX_CHARS {
        match body.rsplit_once('\n') {
            Some((text, footer))
                if footer.starts_with("发送节点：") || footer.starts_with("- 发送节点：") =>
            {
                format!("{text}\n{line}\n{footer}")
            }
            _ => format!("{body}\n{line}"),
        }
    } else {
        body.to_string()
    }
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
