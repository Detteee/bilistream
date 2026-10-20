//! Shared human-readable tracing output. No configuration or worker locks here.

#[cfg(test)]
pub(crate) mod test_capture;

use regex::Regex;
use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::process::ExitStatus;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::{ChronoLocal, FormatTime};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::registry::LookupSpan;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const DIAGNOSTIC_COLUMNS: usize = 240;
const RECORD_COLUMNS: usize = 2048;
const REDACTED: &str = "[redacted]";
const FAILURE_REPORT_EVERY: Duration = Duration::from_secs(5 * 60);

/// Observation only: worker retries and health never consult these fields.
#[derive(Debug, Default)]
pub(crate) struct FailureReport {
    since: Option<Instant>,
    last_report: Option<Instant>,
    failures: u64,
    last_success: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FailureSummary {
    pub(crate) failures: u64,
    pub(crate) elapsed: Duration,
    pub(crate) last_success_age: Option<Duration>,
}

impl fmt::Display for FailureSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "累计失败={}次 持续={} 上次成功={}",
            self.failures,
            human_elapsed(self.elapsed),
            self.last_success_age
                .map(|age| format!("{}前", human_elapsed(age)))
                .unwrap_or_else(|| "未知".to_string())
        )
    }
}

impl FailureReport {
    pub(crate) fn failure(&mut self, now: Instant, count: u64) -> Option<FailureSummary> {
        if count > 0 {
            self.since.get_or_insert(now);
            self.failures = self.failures.saturating_add(count);
        }
        self.pending(now)
    }

    /// A successful partial RSS round is still awaiting a verified full scan;
    /// it can report the pending state without adding any failed attempts.
    pub(crate) fn pending(&mut self, now: Instant) -> Option<FailureSummary> {
        let summary = self.summary(now)?;
        if self
            .last_report
            .is_some_and(|last| now.saturating_duration_since(last) < FAILURE_REPORT_EVERY)
        {
            return None;
        }
        self.last_report = Some(now);
        Some(summary)
    }

    pub(crate) fn success(&mut self, now: Instant) -> Option<FailureSummary> {
        let summary = self.summary(now);
        self.since = None;
        self.last_report = None;
        self.failures = 0;
        self.last_success = Some(now);
        summary
    }

    pub(crate) fn summary(&self, now: Instant) -> Option<FailureSummary> {
        Some(FailureSummary {
            failures: self.failures,
            elapsed: now.saturating_duration_since(self.since?),
            last_success_age: self
                .last_success
                .map(|last| now.saturating_duration_since(last)),
        })
    }
}

/// Both entry paths use this format; only the trusted level may contain ANSI.
pub(crate) struct HumanFormat<T = ChronoLocal> {
    timer: T,
}

impl Default for HumanFormat {
    fn default() -> Self {
        Self::new(ChronoLocal::new("%Y-%m-%d %H:%M:%S%z".to_string()))
    }
}

impl<T> HumanFormat<T> {
    pub(crate) fn new(timer: T) -> Self {
        Self { timer }
    }
}

#[derive(Default)]
struct EventText(String);

impl Visit for EventText {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &format_args!("{value}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name().starts_with("log.") {
            return;
        }
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_debug(field, &format_args!("{value}"));
        let mut source = value.source();
        while let Some(error) = source {
            let _ = write!(self.0, "; caused by {error}");
            source = error.source();
        }
    }
}

impl<S, N, T> FormatEvent<S, N> for HumanFormat<T>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    T: FormatTime,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Format the whole event before sanitizing: per-field/per-write cleanup
        // can miss a wrapped credential or leak the end of a split URL.
        // DefaultFields escapes ESC before formatting its message. Read the
        // raw event here so decoration inside "password" cannot hide the
        // credential name from redaction. No raw bytes reach the output writer.
        let mut fields = EventText::default();
        event.record(&mut fields);
        let text = safe_text(&fields.0, RECORD_COLUMNS);
        let metadata = event.metadata();
        self.timer.format_time(&mut writer)?;
        writer.write_char(' ')?;
        if writer.has_ansi_escapes() {
            let color = match *metadata.level() {
                tracing::Level::ERROR => "\x1b[31m",
                tracing::Level::WARN => "\x1b[33m",
                tracing::Level::INFO => "\x1b[32m",
                tracing::Level::DEBUG => "\x1b[34m",
                tracing::Level::TRACE => "\x1b[35m",
            };
            write!(writer, "{color}{}\x1b[0m", metadata.level())?;
        } else {
            write!(writer, "{}", metadata.level())?;
        }
        writeln!(writer, " [{}] {text}", component(metadata.target()))
    }
}

fn component(target: &str) -> Cow<'_, str> {
    let local = target.strip_prefix("bilistream::").unwrap_or(target);
    let tag = if local == "runtime" || local.starts_with("runtime::") {
        "runtime"
    } else if local == "cluster::yt_index" || local.starts_with("plugins::youtube_index") {
        "yt-index"
    } else if local == "cluster" || local.starts_with("cluster::") {
        "cluster"
    } else if local == "webui" || local.starts_with("webui::") {
        "webui"
    } else if local.starts_with("plugins::ffmpeg") {
        "ffmpeg"
    } else if local.starts_with("plugins::danmaku") {
        "danmaku"
    } else if local.starts_with("plugins::twitch") {
        "twitch"
    } else if local.starts_with("plugins::youtube_discovery::rss") {
        "rss"
    } else if local.starts_with("plugins::youtube_websub") {
        "websub"
    } else if local.starts_with("plugins::youtube_discovery") {
        "yt-discovery"
    } else {
        return Cow::Owned(safe_text(target.rsplit("::").next().unwrap_or(target), 24));
    };
    Cow::Borrowed(tag)
}

/// Redact first, then escape external controls and visibly bound optional text.
pub(crate) fn safe_text(raw: &str, max_columns: usize) -> String {
    // ANSI decoration must not hide part of a credential name from the guard.
    let plain = strip_ansi_codes(raw);
    let redacted = redact(&plain);
    let mut single_line = String::with_capacity(redacted.len());
    for ch in redacted.chars() {
        match ch {
            '\n' => single_line.push_str("\\n"),
            '\r' => single_line.push_str("\\r"),
            '\t' => single_line.push_str("\\t"),
            c if c.is_control()
                || matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                let _ = write!(single_line, "\\u{{{:x}}}", c as u32);
            }
            c => single_line.push(c),
        }
    }
    shorten(&single_line, max_columns)
}

pub(crate) fn safe_diagnostic(raw: &str) -> String {
    let text = safe_text(raw.trim(), DIAGNOSTIC_COLUMNS);
    if text.trim().is_empty() {
        "未提供诊断文本".to_string()
    } else {
        text
    }
}

/// Native stderr readers use this before splitting a possibly wrapped secret
/// into subsequent diagnostic records. Test the original text, before its cap.
pub(crate) fn has_sensitive_data(raw: &str) -> bool {
    SENSITIVE_START.is_match(&strip_ansi_codes(raw))
}

/// A reader cannot reliably distinguish a wrapped credential tail from a bare
/// diagnostic. Keep only recognized error classes until its next safe boundary.
pub(crate) fn safe_diagnostic_continuation(raw: &str) -> String {
    let mut summary = "[诊断续行已省略]".to_string();
    append_error_classes(&mut summary, raw);
    shorten(&summary, DIAGNOSTIC_COLUMNS)
}

fn shorten(text: &str, max_columns: usize) -> String {
    // A width limit alone does not bound long runs of combining characters.
    let max_bytes = max_columns.saturating_mul(8);
    if text.width() <= max_columns && text.len() <= max_bytes {
        return text.to_string();
    }
    if max_columns == 0 {
        return String::new();
    }
    let mut width = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = width + ch.width().unwrap_or(0);
        if next > max_columns - 1 || index + ch.len_utf8() > max_bytes.saturating_sub(3) {
            break;
        }
        width = next;
        end = index + ch.len_utf8();
    }
    // Do not leave a dangling joiner before the explicit omission marker.
    let prefix = text[..end].trim_end_matches('\u{200d}');
    format!("{prefix}…")
}

// Match starts, not entire values: a malformed or wrapped secret must not fall
// back to the original text just because a URL/JSON parser rejects it.
static SENSITIVE_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?ix)
        (?P<url>\b(?:https?|rtmps?|socks(?:4a?|5h?))\s*(?::|%3a)
            |\b(?:[a-z0-9-]+\.)*(?:googlevideo\.com|ttvnw\.net)\b)
        |(?P<private>-----BEGIN[\x20A-Z]*PRIVATE\s+KEY-----)
        |(?P<credential>\b(?:[a-z0-9]+[_-])*(?:authorization|proxy-authorization|set-cookie|cookies?
            |sessdata|bili_jct|user_session|(?:panel[_-]?)?password|passwd
            |(?:access|refresh|id|oauth)[_-]?token|token|api[_-]?key|key
            |(?:rtmp|stream)[_-]?key|streamname|signature|sig|jwt|private[_-]?key)
            \b[\\"']*\s*[:=]\s*)
        |(?P<flag>--(?:password|token|api-key|stream-key|proxy-password)\s+)
        |(?P<auth>\b(?:bearer|basic)\s+)
        "#,
    )
    .expect("valid log redaction pattern")
});

fn redact(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut cursor = 0;
    while let Some(captures) = SENSITIVE_START.captures(&raw[cursor..]) {
        let found = captures.get(0).expect("matched sensitive prefix");
        let start = cursor + found.start();
        let value_start = cursor + found.end();
        result.push_str(&raw[cursor..start]);
        let is_url = captures.name("url").is_some();
        let private_key = captures.name("private").is_some();
        let prefix = &raw[start..value_start];
        if !is_url && !private_key {
            result.push_str(prefix);
        }
        // Credential values can be quoted JSON, Debug's Some("...") or an
        // unlabelled wrapped tail. Keep only safe error classes from that tail.
        let mut end = if is_url {
            value_end(raw, start, value_start)
        } else {
            raw.len()
        };
        if is_url {
            let value = &raw[start..end];
            if let Some(public) = public_log_url(value) {
                result.push_str(&public);
                cursor = end;
                continue;
            }
            // A rejected suspicious URL may contain whitespace/quote fragments.
            // Discard its tail; recover only allowlisted error categories below.
            if reqwest::Url::parse(value)
                .ok()
                .is_none_or(|url| url.host_str().is_none())
            {
                end = raw.len();
            }
            let lower = value.to_ascii_lowercase();
            result.push_str(if lower.starts_with("rtmp") {
                "RTMP URL "
            } else if lower.contains("googlevideo.com") {
                "googlevideo URL "
            } else if lower.contains("ttvnw.net") {
                "ttvnw URL "
            } else if lower.contains("holodex.net") {
                "Holodex URL "
            } else if lower.starts_with("socks") {
                "proxy URL "
            } else {
                "HTTP URL "
            });
        }
        result.push_str(REDACTED);
        if end == raw.len() {
            append_error_classes(&mut result, &raw[start..end]);
        }
        cursor = end;
    }
    result.push_str(&raw[cursor..]);
    result
}

fn value_end(raw: &str, start: usize, value_start: usize) -> usize {
    let tail = &raw[value_start..];
    // Treat physical and Debug-escaped line breaks as possible credential
    // continuations. Never let a tail become a separate apparently safe record.
    if tail.contains(['\n', '\r']) || tail.contains("\\n") || tail.contains("\\r") {
        return raw.len();
    }
    let quote = raw[..start]
        .chars()
        .next_back()
        .filter(|ch| matches!(ch, '\'' | '"'));
    if let Some(quote) = quote {
        let begin = value_start;
        let mut escaped = false;
        for (index, ch) in raw[begin..].char_indices() {
            if ch == quote && !escaped {
                return begin + index;
            }
            escaped = ch == '\\' && !escaped;
        }
        return raw.len();
    }
    tail.char_indices()
        .find(|(_, ch)| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | ')' | '}'))
        .map_or(raw.len(), |(index, _)| value_start + index)
}

fn public_log_url(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    // Startup access instructions remain useful. Only an exact loopback root
    // without credentials, path data, query or fragment can take this route.
    if matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str()?, "localhost" | "127.0.0.1" | "[::1]")
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
    {
        return Some(url.to_string());
    }
    if url.scheme() != "https" {
        return None;
    }
    let safe_id = |id: &str, len: usize| {
        id.len() == len
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    };
    match url.host_str()? {
        "www.youtube.com" | "youtube.com" | "m.youtube.com" if url.path() == "/watch" => {
            let (_, video) = url.query_pairs().find(|(name, _)| name == "v")?;
            safe_id(&video, 11).then(|| format!("https://www.youtube.com/watch?v={video}"))
        }
        "youtu.be" => {
            let video = url.path().strip_prefix('/')?;
            safe_id(video, 11).then(|| format!("https://youtu.be/{video}"))
        }
        "www.youtube.com" | "youtube.com" => {
            let channel = url.path().strip_prefix("/channel/")?;
            (channel.starts_with("UC") && safe_id(channel, 24))
                .then(|| format!("https://www.youtube.com/channel/{channel}"))
        }
        _ => None,
    }
}

static HTTP_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:HTTP(?:/\d(?:\.\d)?)?(?:\s+(?:error|status(?:\s+code)?))?[\s:=]+|Server\s+returned\s+)([1-5]\d\d)\b",
    )
    .expect("valid HTTP status pattern")
});

fn append_error_classes(output: &mut String, omitted: &str) {
    let mut statuses = Vec::new();
    for found in HTTP_STATUS.captures_iter(omitted) {
        let status = found.get(1).expect("matched status code").as_str();
        if !statuses.contains(&status) && statuses.len() < 3 {
            statuses.push(status);
            let _ = write!(output, "; HTTP {status}");
        }
    }
    let lower = omitted.to_ascii_lowercase();
    for (needle, label) in [
        ("broken pipe", "Broken pipe"),
        ("end of file", "End of file"),
        ("connection refused", "Connection refused"),
        ("connection reset", "Connection reset"),
        ("timed out", "Timed out"),
        ("invalid argument", "Invalid argument"),
        ("permission denied", "Permission denied"),
        ("conversion failed", "Conversion failed"),
        ("invalid data found", "Invalid data found"),
        ("input/output error", "Input/output error"),
        ("no route to host", "No route to host"),
        ("no space left on device", "No space left on device"),
        ("protocol not found", "Protocol not found"),
    ] {
        if lower.contains(needle) {
            let _ = write!(output, "; {label}");
        }
    }
}

/// Remove terminal escape sequences, including OSC and incomplete sequences.
/// The formatter adds its trusted severity color only after this boundary.
pub(crate) fn strip_ansi_codes(raw: &str) -> String {
    // Error/Debug wrappers may already have escaped ESC. Normalize those forms
    // before removing decoration, without ever writing the reconstructed bytes.
    let raw = raw
        .replace("\\x1b", "\x1b")
        .replace("\\x1B", "\x1b")
        .replace("\\u{1b}", "\x1b")
        .replace("\\u001b", "\x1b");
    let mut result = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        let kind = if ch == '\x1b' {
            chars.next()
        } else if ch == '\u{009b}' {
            Some('[')
        } else if ch == '\u{009d}' {
            Some(']')
        } else {
            result.push(ch);
            continue;
        };
        match kind {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    result
}

/// Floor to the largest unit and its adjacent unit; retain signed/future ages.
pub(crate) fn human_duration(seconds: i64) -> String {
    let seconds_abs = seconds.unsigned_abs();
    let sign = if seconds < 0 { "-" } else { "" };
    let (major, major_label, minor, minor_label) = if seconds_abs >= 86_400 {
        (
            seconds_abs / 86_400,
            "天",
            seconds_abs % 86_400 / 3600,
            "小时",
        )
    } else if seconds_abs >= 3600 {
        (seconds_abs / 3600, "小时", seconds_abs % 3600 / 60, "分")
    } else if seconds_abs >= 60 {
        (seconds_abs / 60, "分", seconds_abs % 60, "秒")
    } else {
        return format!("{sign}{seconds_abs}秒");
    };
    if minor == 0 {
        format!("{sign}{major}{major_label}")
    } else {
        format!("{sign}{major}{major_label}{minor}{minor_label}")
    }
}

pub(crate) fn human_elapsed(duration: Duration) -> String {
    human_duration(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

pub(crate) fn exit_status(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit={code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal={signal}");
        }
    }
    "exit status unavailable".to_string()
}

#[cfg(test)]
mod tests {
    /// Console prints bypass the tracing redactor, so none of them may format
    /// a B站 cookie or CSRF value (R11). Scans every `print!`-family call in
    /// `src/` up to its terminating `;`.
    #[test]
    fn no_console_print_formats_a_cookie_or_csrf_value() {
        fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    files(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let macros = [
            concat!("print", "ln!("),
            concat!("eprint", "ln!("),
            concat!("print", "!("),
            concat!("eprint", "!("),
        ];
        let forbidden = ["bili_jct", "sessdata", "csrf"];
        let mut paths = Vec::new();
        files(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut paths,
        );
        assert!(paths.len() > 100, "scanned {} files", paths.len());
        let mut offenders = Vec::new();
        for path in paths {
            let text = std::fs::read_to_string(&path).unwrap();
            for name in macros {
                let mut rest = text.as_str();
                while let Some(at) = rest.find(name) {
                    let call = &rest[at..];
                    let call = &call[..call.find(';').unwrap_or(call.len())];
                    let lower = call.to_ascii_lowercase();
                    if forbidden.iter().any(|word| lower.contains(word)) {
                        offenders.push(format!(
                            "{}: {}",
                            path.display(),
                            call.lines().next().unwrap_or("")
                        ));
                    }
                    rest = &rest[at + name.len()..];
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
    }

    use super::*;

    #[test]
    fn failure_reports_bound_repetition_without_losing_counts_or_real_recovery() {
        let now = Instant::now();
        let mut report = FailureReport::default();
        assert!(report.success(now).is_none());
        for attempt in 0..12 {
            let at = now + Duration::from_secs(30 + attempt * 30);
            let summary = report.failure(at, 1);
            assert_eq!(summary.is_some(), attempt == 0 || attempt == 10);
        }
        let recovered = report.success(now + Duration::from_secs(391)).unwrap();
        assert_eq!(recovered.failures, 12);
        assert_eq!(recovered.elapsed, Duration::from_secs(361));
        assert_eq!(recovered.last_success_age, Some(Duration::from_secs(391)));
        assert!(report.success(now + Duration::from_secs(392)).is_none());
        let next = report.failure(now + Duration::from_secs(393), 2).unwrap();
        assert_eq!(next.failures, 2);
        assert_eq!(next.elapsed, Duration::ZERO);
        assert_eq!(next.last_success_age, Some(Duration::from_secs(1)));
        assert!(report.pending(now + Duration::from_secs(394)).is_none());
    }

    #[test]
    fn redacts_urls_headers_tokens_and_malformed_continuations_before_shortening() {
        for raw in [
            "HTTP 403 rtmp://push.invalid/live/synthetic-rtmp?key=synthetic-key",
            "HTTP 502 https://r1.example.googlevideo.com/videoplayback?signature=synthetic-signature",
            "HTTP 403 https://video.example.ttvnw.net/hls/synthetic-media?token=synthetic-token",
            "Connection refused http://synthetic-user:synthetic-password@proxy.invalid:8080",
            "Cookie: SESSDATA=synthetic-cookie; other=synthetic-other",
            "Authorization: Bearer synthetic-bearer",
            "{\"api_key\":\"synthetic-api\",\"password\":\"synthetic-password\"}",
            "streamname=synthetic-stream&key=synthetic-key&status=403",
            "token='synthetic-token\nsynthetic-continuation' Broken pipe",
            "rtmp://push.invalid/live/synthetic-head\n  synthetic-tail HTTP 502 Broken pipe",
            "rtmp: / /broken/synthetic-head synthetic-tail HTTP 530",
            "https%3A%2F%2Fmedia.invalid%2Fsynthetic-token HTTP 403",
            "googlevideo.com/synthetic-path?token=synthetic-token HTTP 403",
            "Auth\x1b[31morization: Bearer synthetic-hidden-name",
            "-----BEGIN PRIVATE KEY-----\nsynthetic-private-key\n-----END PRIVATE KEY-----",
            "--api-key synthetic-flag",
            "Token=\"synthetic-unterminated synthetic-tail",
            "error=\"Cookie: synthetic-head\\nsynthetic-tail\" HTTP 403",
            r#"body="{\"token\":\"synthetic-json-token\"}""#,
            "Config { password: Some(\"synthetic-debug-password\") }",
            "Config { holodex_api_key: Some(\"synthetic-holodex-key\") }",
            r#"{"youtube_api_key":"synthetic-youtube-key"}"#,
            r#"{"node_private_key":"synthetic-node-key"}"#,
            "password=[redacted]synthetic-forged-marker",
            r"pass\u{1b}[31mword=synthetic-debug-decorated-password",
        ] {
            let result = safe_text(raw, 4096);
            assert!(!result.contains("synthetic-"), "{result}");
            assert!(result.contains(REDACTED), "{result}");
            assert_eq!(safe_text(&result, 4096), result, "redaction is idempotent");
        }

        let oversized = format!(
            "jp YT/示例频道001 HTTP 403 rtmp://push.invalid/{}\nsynthetic-tail Broken pipe",
            "synthetic-secret".repeat(10_000)
        );
        let result = safe_diagnostic(&oversized);
        assert!(result.contains("jp YT/示例频道001 HTTP 403"));
        assert!(result.contains("Broken pipe"));
        assert!(!result.contains("synthetic"));
        assert!(result.width() <= DIAGNOSTIC_COLUMNS);
    }

    #[test]
    fn fail_closed_diagnostics_retain_http_and_error_classes() {
        let result = safe_diagnostic(
            "https: //broken/synthetic-part\nsynthetic-tail HTTP 530 HTTP 502 HTTP 530 BROKEN PIPE",
        );
        assert!(!result.contains("synthetic"));
        assert!(result.contains("HTTP 530"));
        assert!(result.contains("HTTP 502"));
        assert!(result.contains("Broken pipe"));
        assert_eq!(result.matches("HTTP 530").count(), 1);
        assert!(has_sensitive_data("ERROR rtmp://push.invalid/live/"));
        assert!(!has_sensitive_data("HTTP 403 Broken pipe"));
        let continuation =
            safe_diagnostic_continuation("synthetic-wrapped-tail HTTP 403 BROKEN PIPE");
        assert_eq!(continuation, "[诊断续行已省略]; HTTP 403; Broken pipe");
        assert_eq!(
            safe_diagnostic_continuation("synthetic-tail Server returned 403 Forbidden; No space left on device; Protocol not found"),
            "[诊断续行已省略]; HTTP 403; No space left on device; Protocol not found"
        );
    }

    #[test]
    fn preserves_allowlisted_public_identity_without_extra_query_credentials() {
        assert_eq!(
            safe_diagnostic(
                "video https://www.youtube.com/watch?v=example0001&token=synthetic-token"
            ),
            "video https://www.youtube.com/watch?v=example0001"
        );
        assert_eq!(
            safe_text("channel=UC_example_001 video=example0001", 80),
            "channel=UC_example_001 video=example0001"
        );
        assert!(!safe_diagnostic(
            "https://synthetic-user:synthetic-password@www.youtube.com/watch?v=example0001"
        )
        .contains("synthetic"));
    }

    #[test]
    fn preserves_only_credential_free_loopback_root_access_links() {
        for url in [
            "http://localhost:3150/",
            "http://127.0.0.1:3150/",
            "http://[::1]:3150/",
            "https://localhost:3150/",
        ] {
            assert_eq!(
                safe_diagnostic(&format!("访问 {url}")),
                format!("访问 {url}")
            );
        }
        for url in [
            "http://synthetic-user:synthetic-password@localhost:3150/",
            "http://localhost:3150/?token=synthetic-token",
            "http://127.0.0.1:3150/synthetic-media-path",
            "http://localhost:3150/#synthetic-fragment",
            "http://localhost.invalid:3150/",
            "https://peer.invalid/",
            "rtmp://127.0.0.1:3150/",
        ] {
            let safe = safe_diagnostic(&format!("连接失败 {url}"));
            assert!(safe.contains(REDACTED), "{safe}");
            assert!(!safe.contains(url));
            assert!(!safe.contains("synthetic"));
        }
    }

    #[test]
    fn one_line_text_keeps_unicode_and_makes_control_input_inert() {
        let result = safe_text(
            "示例频道001😀\nINFO forged\r\t\x07\u{202e}ERROR\x1b[31m红色\x1b[0m",
            100,
        );
        assert!(result.starts_with("示例频道001😀\\nINFO forged\\r\\t\\u{7}\\u{202e}ERROR红色"));
        assert!(!result.chars().any(char::is_control));
        let shortened = safe_text(&"示例频道001😀".repeat(100), 31);
        assert!(shortened.ends_with('…'));
        assert!(shortened.width() <= 31);
        assert!(!shortened.contains('\u{fffd}'));
        assert!(safe_text(&"\u{0301}".repeat(10_000), 20).len() <= 160);
        assert_eq!(safe_text("long", 0), "");
        assert_eq!(safe_text("long", 1), "…");
        assert_eq!(safe_diagnostic(" \n \t "), "未提供诊断文本");
        assert_eq!(safe_diagnostic("\x1b[31m \x1b[0m"), "未提供诊断文本");
    }

    #[test]
    fn components_are_short_stable_and_do_not_require_runtime_state() {
        for (target, expected) in [
            ("bilistream::runtime::launch", "runtime"),
            ("bilistream::plugins::ffmpeg", "ffmpeg"),
            ("bilistream::plugins::danmaku_client", "danmaku"),
            ("bilistream::cluster::heartbeat", "cluster"),
            ("bilistream::cluster::yt_index", "yt-index"),
            ("bilistream::plugins::youtube_index", "yt-index"),
            ("bilistream::plugins::youtube_discovery::rss", "rss"),
            ("bilistream::plugins::youtube_websub", "websub"),
            ("bilistream::webui::api::setup", "webui"),
            ("dependency::transport", "transport"),
        ] {
            assert_eq!(component(target), expected);
        }
        assert!(component("dependency::a_very_long_final_component_name").width() <= 24);
    }

    #[test]
    fn signed_human_duration_has_consistent_unit_boundaries() {
        for (seconds, expected) in [
            (0, "0秒"),
            (59, "59秒"),
            (60, "1分"),
            (63, "1分3秒"),
            (3599, "59分59秒"),
            (3600, "1小时"),
            (3721, "1小时2分"),
            (86399, "23小时59分"),
            (86400, "1天"),
            (4_288_560, "49天15小时"),
            (-59, "-59秒"),
            (-63, "-1分3秒"),
            (-86400, "-1天"),
        ] {
            assert_eq!(human_duration(seconds), expected);
        }
        assert!(human_duration(i64::MIN).starts_with('-'));
    }

    #[cfg(unix)]
    #[test]
    fn exit_status_reports_numeric_code_or_signal() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit_status(&ExitStatus::from_raw(7 << 8)), "exit=7");
        assert_eq!(exit_status(&ExitStatus::from_raw(15)), "signal=15");
    }
}
