//! Message rendering (design §10, alert format spec
//! `/workspace/qq-notify-alert-formats.md`).
//!
//! - First line = marker, title and subject (`🔴 节点故障 · sg-3`).
//! - Plain text by default, at most 500 characters. When too long, lines
//!   are dropped in a fixed order (§10.2); the first line, the time line and
//!   the 未送达 line are never dropped.
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
pub const LABEL_DELEGATE: &str = "（代发）";

#[derive(Clone, Debug, Default)]
pub struct RenderContext {
    pub now: u64,
    /// Node id of the sending node.
    pub reporter: String,
    pub active_owner: Option<String>,
    /// `（无多数派）` / `（代发）`, prefixed to the first line.
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

/// Delink, full-width `<>`, then `safe_text` (redaction, control chars,
/// width cut).
pub fn sanitize(raw: &str, cols: usize) -> String {
    let cleaned = fullwidth_angles(&delink(raw));
    crate::logging::safe_text(&cleaned, cols)
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

fn title(item: &OutItem) -> &'static str {
    if item.kind == EventKind::NodeDegraded && LATCHED_REASONS.contains(&variant(item)) {
        "节点自我隔离"
    } else {
        item.kind.title()
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

fn held_line(item: &OutItem, ctx: &RenderContext) -> String {
    let duration = match item.item {
        ItemKind::Resolved { duration_secs } => duration_text(duration_secs),
        ItemKind::Alert => String::new(),
    };
    format!(
        "· ✅ {} {} 已恢复（{}，{} 恢复）",
        title(item),
        ctx.node_name(&item.node),
        duration,
        short_time(item.queued_at, ctx.now)
    )
}

fn summary_line(item: &OutItem, ctx: &RenderContext) -> String {
    match &item.item {
        ItemKind::Resolved { duration_secs } => format!(
            "· ✅ {} {} 已恢复（{}）",
            title(item),
            ctx.node_name(&item.node),
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
            format!("· 🟢 开播 {} · 来源 {}", subject(item, ctx), platform)
        }
        ItemKind::Alert => {
            let mut line = format!("· {} {} {}", marker(item), title(item), subject(item, ctx));
            if item.flapping.is_some() {
                line.push_str(&format!(
                    "（{} 分钟内出现 {} 次）",
                    super::policy::FLAP_WINDOW_SECS / 60,
                    item.flapping.unwrap_or_default()
                ));
            } else if let Some(reason) = reason(item, ctx, SUMMARY_REASON_COLS) {
                line.push_str(&format!(" · {reason}"));
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
    owner_full: String,
    owner_short: String,
    undelivered: Option<String>,
    markdown: bool,
}

impl Parts {
    fn assemble(&self, body: &[String], max_items: usize, owner: &str) -> String {
        let mut lines = vec![self.first.clone(), self.time.clone()];
        lines.extend(body.iter().cloned());
        lines.extend(self.items.iter().take(max_items).cloned());
        if self.items.len() > max_items {
            lines.push(format!("另有 {} 条", self.items.len() - max_items));
        }
        lines.push(owner.to_string());
        if let Some(line) = &self.undelivered {
            lines.push(line.clone());
        }
        decorate(lines, self.markdown)
    }

    /// Drop order (spec §10.2): CTA (appended later, only if it fits),
    /// the raw text in brackets, summary entries 5 → 3, the owner line
    /// shortened to 「报告 …」, then a hard cut of the middle lines.
    fn fit(&self) -> String {
        let full = self.assemble(&self.body, MAX_LINES, &self.owner_full);
        if full.chars().count() <= MAX_CHARS {
            return full;
        }
        let stripped: Vec<String> = self.body.iter().map(|line| strip_brackets(line)).collect();
        let text = self.assemble(&stripped, MAX_LINES, &self.owner_full);
        if text.chars().count() <= MAX_CHARS {
            return text;
        }
        let text = self.assemble(&stripped, MIN_LINES, &self.owner_full);
        if text.chars().count() <= MAX_CHARS {
            return text;
        }
        let text = self.assemble(&stripped, MIN_LINES, &self.owner_short);
        if text.chars().count() <= MAX_CHARS {
            return text;
        }
        // Hard cut, but only of the middle: the first line, the time and the
        // 未送达 line always survive.
        let fixed = self.assemble(&[], 0, &self.owner_short);
        let fixed_len = fixed.chars().count();
        let mut middle: Vec<String> = stripped.clone();
        middle.extend(self.items.iter().take(MIN_LINES).cloned());
        let middle = decorate_tail(middle, self.markdown).join("\n");
        let budget = MAX_CHARS.saturating_sub(fixed_len + 2);
        let cut: String = middle.chars().take(budget.saturating_sub(1)).collect();
        let mut lines: Vec<&str> = fixed.lines().collect();
        let cut_line = format!("{cut}…");
        if budget > 1 {
            lines.insert(2, &cut_line);
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
    let label = ctx
        .label
        .clone()
        .map(|label| {
            if label.starts_with('（') {
                label
            } else {
                format!("（{label}）")
            }
        })
        .unwrap_or_default();
    let reporter = ctx.node_name(&ctx.reporter);
    let owner = ctx
        .active_owner
        .as_deref()
        .map(|owner| ctx.node_name(owner));
    let owner_full = match &owner {
        Some(owner) => format!("当前活跃节点 {owner} · 报告 {reporter}"),
        None => format!("报告 {reporter}"),
    };
    let owner_short = format!("报告 {reporter}");
    let undelivered = (ctx.undelivered > 0).then(|| {
        let reason = ctx
            .undelivered_reason
            .as_deref()
            .map(|reason| format!("（{}）", ctx.value(reason, 60)))
            .unwrap_or_default();
        format!("另有 {} 条通知未送达{}", ctx.undelivered, reason)
    });
    let time = format!("时间 {} (UTC+8)", utc8_time(ctx.now));
    let held: Vec<String> = ctx.held.iter().map(|item| held_line(item, ctx)).collect();
    let mut parts = Parts {
        first: String::new(),
        time,
        body: Vec::new(),
        items: Vec::new(),
        owner_full,
        owner_short,
        undelivered,
        markdown: ctx.markdown,
    };
    match batch {
        [] if held.is_empty() => return String::new(),
        [] => {
            parts.first = format!("{label}✅ 已恢复（{} 条）", held.len());
            parts.items = held;
        }
        [item] => {
            single(item, ctx, &label, &reporter, owner.as_deref(), &mut parts);
            parts.items = held;
        }
        items => {
            summary(items, ctx, &label, &mut parts);
            parts.items.extend(held);
        }
    }
    parts.fit()
}

fn single(
    item: &OutItem,
    ctx: &RenderContext,
    label: &str,
    reporter: &str,
    owner: Option<&str>,
    parts: &mut Parts,
) {
    let subject = subject(item, ctx);
    match (&item.item, item.kind) {
        (ItemKind::Resolved { duration_secs }, _) => {
            let mut extra = format!("持续 {}", duration_text(*duration_secs));
            if item.suppressed > 0 {
                extra.push_str(&format!("，期间重复 {} 次", item.suppressed));
            }
            parts.first = format!(
                "{label}✅ 已恢复 · {} · {}（{}）",
                title(item),
                subject,
                extra
            );
        }
        (_, EventKind::Test) => {
            parts.first = format!("{label}🧪 通知测试 · {reporter}");
            parts.body = vec!["这是一条 bilistream 测试消息，收到即说明 QQ 通知可用。".to_string()];
            parts.owner_full = match owner {
                Some(owner) => format!("发送节点 {reporter} · 当前活跃节点 {owner}"),
                None => format!("发送节点 {reporter}"),
            };
            parts.owner_short = format!("发送节点 {reporter}");
        }
        (_, EventKind::StreamLive) => {
            parts.first = format!("{label}🟢 开播 · {subject}");
            if let Some(live) = &item.extra.live {
                parts.body.push(format!(
                    "来源 {}",
                    live.platforms
                        .iter()
                        .map(|p| platform_name(p))
                        .collect::<Vec<_>>()
                        .join("、")
                ));
                if let Some(stream_title) = live.title.as_deref().filter(|t| !t.trim().is_empty()) {
                    parts
                        .body
                        .push(format!("标题 {}", ctx.value(stream_title, DETAIL_COLS)));
                }
            }
        }
        (_, EventKind::Failover) => {
            parts.first = format!("{label}🔴 故障转移 · {subject}");
            let old = item
                .extra
                .old_owner
                .as_deref()
                .map(|old| ctx.node_name(old))
                .unwrap_or_default();
            if let Some(reason) = reason(item, ctx, DETAIL_COLS) {
                parts
                    .body
                    .push(format!("原因 {old} {reason}").replace("  ", " "));
            }
            parts
                .body
                .push(format!("推流已由 {} 接管", ctx.node_name(&item.node)));
        }
        _ if item.flapping.is_some() => {
            parts.first = format!(
                "{label}🟠 节点状态反复 · {} · {}（{} 分钟内出现 {} 次）",
                title(item),
                subject,
                super::policy::FLAP_WINDOW_SECS / 60,
                item.flapping.unwrap_or_default()
            );
        }
        _ => {
            let mut first = format!("{label}{} {} · {}", marker(item), title(item), subject);
            if item.suppressed > 0 {
                first.push_str(&format!("（期间重复 {} 次）", item.suppressed));
            }
            parts.first = first;
            if let Some(reason) = reason(item, ctx, DETAIL_COLS) {
                let lasted = ctx.now.saturating_sub(item.incident_at);
                if lasted >= 30 {
                    parts
                        .body
                        .push(format!("原因 {reason}（已持续 {}）", duration_text(lasted)));
                } else {
                    parts.body.push(format!("原因 {reason}"));
                }
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
        format!("故障转移 · {}", subject(item, ctx))
    } else if all_resolved {
        format!("全部恢复（{} 条）", items.len())
    } else if all_live {
        format!("开播（{} 个频道）", alerts.len())
    } else if same_node {
        format!(
            "{} · {}（{} 项）",
            title(alerts[0]),
            ctx.node_name(&alerts[0].node),
            alerts.len()
        )
    } else {
        format!("集群异常（{} 条）", items.len())
    };
    parts.first = format!("{label}{head_marker} {incident_title}");
    if let Some(start) = items.iter().map(|item| item.incident_at).min() {
        if utc8(start, "%Y-%m-%d %H:%M") != utc8(ctx.now, "%Y-%m-%d %H:%M") {
            parts.time = format!("{} · 开始于 {}", parts.time, short_time(start, ctx.now));
        }
    }
    parts.items = items.iter().map(|item| summary_line(item, ctx)).collect();
}

/// Last-resort body when QQ rejects the content (design §7 ContentRejected).
/// It still carries the 未送达 count, so a delivery always reports it.
pub fn minimal(batch: &[OutItem], undelivered: u32) -> String {
    let critical = batch
        .iter()
        .filter(|item| item.severity() == Severity::Critical && item.item == ItemKind::Alert)
        .count();
    let mut text = format!(
        "bilistream 通知 {} 条（严重 {} 条），请查看面板",
        batch.len().max(1),
        critical
    );
    if undelivered > 0 {
        text.push_str(&format!("；另有 {undelivered} 条通知未送达"));
    }
    text
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
        format!("{body}\n{line}")
    } else {
        body.to_string()
    }
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
