use crate::logging::{exit_status, safe_diagnostic, safe_text};
use reqwest_middleware::ClientBuilder;
use reqwest_middleware::ClientWithMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;
use serde_json::json;
use std::error::Error;
use std::future::Future;
use std::io;
use std::process::{Command, Output};
use std::time::Duration;

use super::subprocess::{command_output_with_timeout, configure_no_window};
use crate::plugins::source_status::SourceStatus;

const STREAMLINK_TIMEOUT: Duration = Duration::from_secs(45);
/// The Twitch web client's public GQL client ID.
pub(crate) const GQL_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

/// Return the canonical Twitch channel ID (login used by stream probes) from
/// an absolute Twitch HTTP(S) URL.
///
/// Only `twitch.tv` and `www.twitch.tv` are accepted. The first
/// path segment supplies a nonempty login; later segments, query, and fragment do
/// not affect identity. Callers retain the original URL for display.
pub(crate) fn channel_id_from_url(link: &str) -> Option<String> {
    let url = reqwest::Url::parse(link.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    if !host.eq_ignore_ascii_case("twitch.tv") && !host.eq_ignore_ascii_case("www.twitch.tv") {
        return None;
    }
    let login = url.path_segments()?.next()?.trim();
    (!login.is_empty()).then(|| login.to_ascii_lowercase())
}

pub struct Twitch {
    pub channel_id: String,
    pub client: ClientWithMiddleware,
    pub proxy_region: String,
    pub proxy: Option<String>,
}

impl Twitch {
    pub fn new(
        channel_id: &str,
        proxy_region: String,
        proxy: Option<String>,
    ) -> Result<Self, reqwest::Error> {
        // 设置最大重试次数为5次
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);
        let raw_client = reqwest::Client::builder()
            .cookie_store(true)
            // 设置超时时间为30秒
            .timeout(Duration::new(30, 0))
            .build()?;
        let client = ClientBuilder::new(raw_client.clone())
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .build();

        Ok(Twitch {
            channel_id: channel_id.to_string(),
            client,
            proxy_region,
            proxy,
        })
    }

    pub async fn get_status(&self) -> Result<SourceStatus, Box<dyn Error>> {
        self.get_status_mode(false).await
    }

    /// The push loop's m3u8 refetch: "not live" only when Twitch returns the
    /// user with no live stream; anything else is an error.
    pub async fn get_status_refetch(&self) -> Result<SourceStatus, Box<dyn Error>> {
        self.get_status_mode(true).await
    }

    async fn get_status_mode(&self, strict: bool) -> Result<SourceStatus, Box<dyn Error>> {
        let (is_live, game_name, title, stream_id) =
            get_twitch_status_mode(&self.channel_id, strict).await?;
        if is_live {
            let cfg = crate::config::load_config().await?;
            let quality = cfg.twitch.quality.clone();
            let m3u8_url = self
                .get_streamlink_url(Some(&quality), &cfg.cluster.node_id)
                .await?;
            Ok(SourceStatus {
                is_live,
                topic: Some(game_name.unwrap_or_default()),
                title: Some(title.unwrap_or_default()),
                m3u8_url: Some(m3u8_url),
                start_time: None,
                video_id: stream_id,
            })
        } else {
            Ok(SourceStatus {
                is_live,
                topic: None,
                title: None,
                m3u8_url: None,
                start_time: None,
                video_id: stream_id,
            })
        }
    }
    async fn get_streamlink_url(
        &self,
        quality: Option<&str>,
        node: &str,
    ) -> Result<String, Box<dyn Error>> {
        streamlink_regions(
            node,
            &self.channel_id,
            &self.proxy_region,
            |region| async move { self.try_with_proxy(&region, quality).await },
        )
        .await
    }

    async fn try_with_proxy(
        &self,
        proxy_region: &str,
        quality: Option<&str>,
    ) -> Result<String, Box<dyn Error>> {
        let proxy_url = self.get_proxy_url_for_region(proxy_region)?;
        let quality = quality.unwrap_or("best");

        let mut cmd = Command::new("streamlink");
        configure_no_window(&mut cmd);
        // Add HTTP proxy if configured
        if let Some(ref proxy) = self.proxy {
            if !proxy.is_empty() {
                cmd.arg("--http-proxy").arg(proxy);
            }
        }
        cmd.arg(proxy_url)
            .arg("--stream-url")
            .arg("--stream-type")
            .arg("hls");

        cmd.arg(format!(
            "https://www.twitch.tv/{}",
            self.channel_id.as_str().replace("\"", "")
        ))
        .arg(quality);

        let output = command_output_with_timeout(cmd, STREAMLINK_TIMEOUT, "streamlink")
            .await
            .map_err(streamlink_execution_error)?;
        streamlink_output(output)
    }

    fn get_proxy_url_for_region(&self, region: &str) -> Result<String, &'static str> {
        match region {
            "na" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com".to_string()),
            "eu" => Ok("--twitch-proxy-playlist=https://lb-eu.cdn-perfprod.com".to_string()),
            "eu2" => Ok("--twitch-proxy-playlist=https://lb-eu2.cdn-perfprod.com".to_string()),
            "eu3" => Ok("--twitch-proxy-playlist=https://lb-eu3.cdn-perfprod.com".to_string()),
            "eu4" => Ok("--twitch-proxy-playlist=https://lb-eu4.cdn-perfprod.com".to_string()),
            "eu5" => Ok("--twitch-proxy-playlist=https://lb-eu5.cdn-perfprod.com".to_string()),
            "as" => Ok("--twitch-proxy-playlist=https://lb-as.cdn-perfprod.com".to_string()),
            "sa" => Ok("--twitch-proxy-playlist=https://lb-sa.cdn-perfprod.com".to_string()),
            "eul" => Ok("--twitch-proxy-playlist=https://eu.luminous.dev".to_string()),
            "eu2l" => Ok("--twitch-proxy-playlist=https://eu2.luminous.dev".to_string()),
            "asl" => Ok("--twitch-proxy-playlist=https://as.luminous.dev".to_string()),
            "all" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com,https://eu.luminous.dev,https://eu2.luminous.dev,https://as.luminous.dev".to_string()),
            "perf" => Ok("--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com,https://lb-eu3.cdn-perfprod.com,https://lb-eu.cdn-perfprod.com,https://lb-eu2.cdn-perfprod.com,https://lb-eu4.cdn-perfprod.com,https://lb-eu5.cdn-perfprod.com".to_string()),
            "" => Ok(String::new()),
            _ => Err("代理区域无效"),
        }
    }
}

/// Own the existing configured-region/backup sequence and its one final result.
/// The attempt closure is also used by deterministic tests; it does not alter
/// subprocess admission, timeout, output limits, or successful-empty semantics.
async fn streamlink_regions<F, Fut>(
    node: &str,
    channel: &str,
    configured: &str,
    mut attempt: F,
) -> Result<String, Box<dyn Error>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<String, Box<dyn Error>>>,
{
    let identity = format!(
        "{} [TW] {}",
        crate::logging::node_tag(node),
        safe_text(channel, 64)
    );
    let backup_regions = ["asl", "as", "na", "sa", "eu", "eu3"];
    let regions = std::iter::once(configured).chain(
        backup_regions
            .into_iter()
            .filter(|region| *region != configured),
    );
    let mut attempted = Vec::new();
    let mut failures = Vec::new();
    for region in regions {
        let label = safe_text(if region.is_empty() { "direct" } else { region }, 32);
        attempted.push(label.clone());
        match attempt(region.to_string()).await {
            Ok(url) => {
                if url.is_empty() {
                    tracing::warn!(
                        "{} streamlink 执行成功但未返回取流地址；region={}；已尝试=[{}]",
                        identity,
                        label,
                        attempted.join(",")
                    );
                } else if attempted.len() > 1 {
                    tracing::info!(
                        "{} streamlink 取流已恢复；region={}；已尝试=[{}]",
                        identity,
                        label,
                        attempted.join(",")
                    );
                }
                return Ok(url);
            }
            Err(error) => {
                let reason = safe_diagnostic(&error.to_string());
                if attempted.len() == 1 {
                    tracing::warn!(
                        "{} streamlink 取流失败；region={}；{}；尝试备用地区",
                        identity,
                        label,
                        reason
                    );
                } else {
                    tracing::debug!(
                        "{} streamlink 取流失败；region={}；{}",
                        identity,
                        label,
                        reason
                    );
                }
                failures.push(format!("{}: {}", label, safe_text(&reason, 160)));
            }
        }
    }
    let summary = format!(
        "{} streamlink 所有地区取流失败；已尝试=[{}]；{}",
        identity,
        attempted.join(","),
        failures.join("；")
    );
    // The monitor can turn a failed get_status into an offline observation, so
    // this collector owns the terminal record as well as the complete error.
    tracing::error!("{summary}");
    Err(summary.into())
}

fn streamlink_execution_error(error: io::Error) -> Box<dyn Error> {
    if error.kind() == io::ErrorKind::NotFound {
        return "streamlink 未安装或不在 PATH 中。（启动失败）".into();
    }
    let stage = match error.kind() {
        io::ErrorKind::TimedOut => "执行超时",
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => "输出读取失败",
        io::ErrorKind::Interrupted => "执行已中断",
        _ => "子进程执行失败",
    };
    Box::new(io::Error::new(
        error.kind(),
        format!(
            "streamlink {stage}：{}",
            safe_diagnostic(&error.to_string())
        ),
    ))
}

fn streamlink_output(output: Output) -> Result<String, Box<dyn Error>> {
    if output.status.success() {
        // Do not introduce URL validation or fallback on an empty successful
        // stdout: those would change the existing acquisition contract.
        return Ok(String::from_utf8(output.stdout)?.trim().to_string());
    }
    let status = exit_status(&output.status);
    let stderr = String::from_utf8(output.stderr).map_err(|error| {
        format!(
            "streamlink {status}；stderr UTF-8 解码失败：{}",
            safe_diagnostic(&error.to_string())
        )
    })?;
    if stderr.contains("usage: streamlink [OPTIONS] <URL> [STREAM]") {
        return Err(
            format!("Streamlink plugin (streamlink-ttvlol) 未安装。streamlink {status}").into(),
        );
    }
    let reason = if stderr.trim().is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lower = stdout.to_ascii_lowercase();
        let has_diagnostic = [
            "error",
            "warning",
            "failed",
            "http ",
            "timeout",
            "timed out",
            "unable",
            "no playable",
            "broken pipe",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
        if has_diagnostic {
            format!("stderr 为空；stdout: {}", safe_diagnostic(&stdout))
        } else {
            safe_diagnostic("")
        }
    } else {
        safe_diagnostic(&stderr)
    };
    Err(format!("streamlink {status}；{reason}").into())
}

type TwitchGqlStatus = (bool, Option<String>, Option<String>, Option<String>);

pub async fn get_twitch_status(
    channel_id: &str,
) -> Result<TwitchGqlStatus, Box<dyn std::error::Error>> {
    get_twitch_status_mode(channel_id, false).await
}

/// Status-only observation for a held source: no streamlink URL lookup and no
/// missing/invalid GraphQL shape misreported as offline.
pub(crate) async fn get_twitch_warning_status(
    channel_id: &str,
) -> Result<TwitchGqlStatus, Box<dyn std::error::Error>> {
    get_twitch_status_mode(channel_id, true).await
}

/// Reads the GetStreamInfo reply. In strict mode only a returned user whose
/// `stream` is null or not `live` counts as not live; `data.user == null` or a
/// reply without the expected shape is an error.
fn parse_twitch_stream_status(
    json_response: &serde_json::Value,
    strict: bool,
) -> Result<TwitchGqlStatus, Box<dyn std::error::Error>> {
    if strict {
        let user = json_response
            .get("data")
            .and_then(|data| data.get("user"))
            .filter(|user| user.is_object())
            .ok_or("Twitch GraphQL returned no user")?;
        match user.get("stream") {
            Some(stream) if stream.is_null() => {}
            Some(stream)
                if stream.is_object()
                    && stream
                        .get("type")
                        .and_then(|kind| kind.as_str())
                        .is_some_and(|kind| !kind.trim().is_empty()) => {}
            _ => return Err("Twitch GraphQL 返回缺少 stream 字段".into()),
        }
    }
    let is_live = json_response["data"]["user"]["stream"]["type"] == "live";
    let game_name = json_response["data"]["user"]["stream"]["game"]["name"]
        .as_str()
        .unwrap_or("");
    let title = json_response["data"]["user"]["stream"]["title"]
        .as_str()
        .unwrap_or("");
    let stream_id = json_response["data"]["user"]["stream"]["id"]
        .as_str()
        .map(|s| s.to_string());
    Ok((
        is_live,
        Some(game_name.to_string()),
        Some(title.to_string()),
        stream_id,
    ))
}

async fn get_twitch_status_mode(
    channel_id: &str,
    strict: bool,
) -> Result<
    (
        bool,           // is_live
        Option<String>, // topic
        Option<String>, // title
        Option<String>, // stream_id
    ),
    Box<dyn std::error::Error>,
> {
    let client = super::http::pooled_client(None)?;

    let query = r#"
    query GetStreamInfo($login: String!) {
        user(login: $login) {
            stream {
                id
                game {
                    id
                    name
                    displayName
                }
                title
                type
                viewersCount
                language
                tags {
                    id
                    localizedName
                }
            }
        }
    }"#;

    let variables = json!({
        "login": channel_id
    });

    let response = client
        .post("https://gql.twitch.tv/gql")
        .timeout(Duration::from_secs(15))
        .header("Client-ID", GQL_CLIENT_ID)
        .json(&json!({
            "query": query,
            "variables": variables
        }))
        .send()
        .await?;

    let json_response: serde_json::Value =
        super::http::response_json_limited(response.error_for_status()?).await?;
    if json_response
        .get("errors")
        .is_some_and(|errors| errors.as_array().is_some_and(|errors| !errors.is_empty()))
    {
        return Err("Twitch GraphQL 返回错误".into());
    }
    parse_twitch_stream_status(&json_response, strict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;

    fn capture() -> (
        tracing::Dispatch,
        crate::logging::test_capture::CapturedLogs,
    ) {
        crate::logging::test_capture::subscriber(tracing::Level::DEBUG)
    }

    fn output(code: u32, stdout: &[u8], stderr: &[u8]) -> Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw((code as i32) << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code)
        };
        Output {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn failed_output_keeps_exit_and_a_nonempty_safe_diagnostic() {
        for stderr in [b"".as_slice(), b" \r\n\t".as_slice()] {
            let error = streamlink_output(output(7, b"", stderr))
                .unwrap_err()
                .to_string();
            assert!(error.contains("exit=7"));
            assert!(error.contains("未提供诊断文本"));
        }
        let stdout = b"[cli][ERROR] Unable to open stream: HTTP 403 https://video.ttvnw.net/hls/SYNTHETIC-STREAM?token=SYNTHETIC-TOKEN";
        let error = streamlink_output(output(1, stdout, b" "))
            .unwrap_err()
            .to_string();
        assert!(error.contains("stdout:"));
        assert!(error.contains("HTTP 403"));
        assert!(!error.contains("SYNTHETIC"));

        let url_only = b"https://video.ttvnw.net/hls/SYNTHETIC-STREAM?token=SYNTHETIC-TOKEN";
        let error = streamlink_output(output(2, url_only, b""))
            .unwrap_err()
            .to_string();
        assert!(error.contains("未提供诊断文本"));
        assert!(!error.contains("SYNTHETIC"));
        let error = streamlink_output(output(3, b"", &[0xff]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exit=3"));
        assert!(error.contains("stderr UTF-8 解码失败"));
    }

    #[test]
    fn plugin_classification_and_successful_stdout_behavior_stay_intact() {
        let error = streamlink_output(output(
            2,
            b"",
            b"usage: streamlink [OPTIONS] <URL> [STREAM]",
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("Streamlink plugin (streamlink-ttvlol) 未安装"));
        assert!(error.contains("exit=2"));
        assert_eq!(
            streamlink_output(output(0, b" \n", b"warning")).unwrap(),
            ""
        );
        assert_eq!(
            streamlink_output(output(0, b" https://example.invalid/watch \n", b"")).unwrap(),
            "https://example.invalid/watch"
        );
        assert!(streamlink_output(output(0, &[0xff], b"")).is_err());

        let error = streamlink_execution_error(io::Error::new(
            io::ErrorKind::TimedOut,
            "streamlink timed out",
        ));
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(error.to_string().contains("执行超时"));
        assert!(
            streamlink_execution_error(io::Error::from(io::ErrorKind::NotFound))
                .to_string()
                .contains("未安装或不在 PATH")
        );
    }

    #[tokio::test]
    async fn exhausted_regions_emit_one_safe_terminal_summary_in_existing_order() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let attempted = Arc::clone(&calls);
        let (dispatch, bytes) = capture();
        let error = streamlink_regions("ca", "example_channel_001", "na", move |region| {
            attempted.lock().unwrap().push(region);
            std::future::ready(Err::<String, Box<dyn Error>>(
                "streamlink exit=1 HTTP 502 Authorization: Bearer SYNTHETIC-TOKEN".into(),
            ))
        })
        .with_subscriber(dispatch)
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            *calls.lock().unwrap(),
            ["na", "asl", "as", "sa", "eu", "eu3"]
        );
        assert!(error.contains("已尝试=[na,asl,as,sa,eu,eu3]"));
        assert!(!error.contains("SYNTHETIC-TOKEN"));
        let output = bytes.text();
        assert_eq!(
            output
                .lines()
                .filter(|line| line.contains(" WARN ["))
                .count(),
            1
        );
        assert_eq!(
            output
                .lines()
                .filter(|line| line.contains(" ERROR ["))
                .count(),
            1
        );
        assert!(output.contains("[ca] [TW] example_channel_001"));
        assert!(output.contains("region=na"));
        assert!(!output.contains("SYNTHETIC-TOKEN"));
    }

    #[tokio::test]
    async fn backup_recovery_and_empty_configured_success_do_not_add_attempts() {
        let (dispatch, bytes) = capture();
        let mut attempts = Vec::new();
        let result = streamlink_regions("jp", "example_channel_002", "as", |region| {
            let first = attempts.is_empty();
            attempts.push(region);
            std::future::ready(if first {
                Err::<String, Box<dyn Error>>("streamlink exit=1".into())
            } else {
                Ok("synthetic-url".into())
            })
        })
        .with_subscriber(dispatch)
        .await
        .unwrap();
        assert_eq!(result, "synthetic-url");
        assert_eq!(attempts, ["as", "asl"]);
        let output = bytes.text();
        assert_eq!(output.matches("取流已恢复").count(), 1);
        assert!(output.contains("已尝试=[as,asl]"));
        assert!(!output.contains("synthetic-url"));

        let mut attempts = Vec::new();
        let (dispatch, empty_logs) = capture();
        let empty = streamlink_regions("jp", "example_channel_002", "", |region| {
            attempts.push(region);
            std::future::ready(Ok::<String, Box<dyn Error>>(String::new()))
        })
        .with_subscriber(dispatch)
        .await
        .unwrap();
        assert!(empty.is_empty());
        assert_eq!(attempts, [""]);
        assert!(empty_logs.text().contains("执行成功但未返回取流地址"));
        assert!(!empty_logs.text().contains("取流已恢复"));
    }

    #[test]
    fn absolute_twitch_urls_have_one_canonical_identity() {
        for url in [
            "https://twitch.tv/example_channel_001",
            "https://www.twitch.tv/Example_Channel_001",
            "HTTP://TWITCH.TV/EXAMPLE_CHANNEL_001",
            "http://www.twitch.tv/example_channel_001/videos",
            " https://www.twitch.tv/Example_Channel_001?x=1#live ",
            "https://twitch.tv/example_channel_001/videos?x=1#live",
        ] {
            assert_eq!(
                channel_id_from_url(url).as_deref(),
                Some("example_channel_001"),
                "{url}"
            );
        }
    }

    #[test]
    fn unrelated_hosts_empty_paths_and_non_absolute_urls_have_no_login() {
        for url in [
            "",
            " ",
            "https://twitch.tv",
            "https://twitch.tv/",
            "https://twitch.tv//example",
            "https://example.com/twitch.tv/example_channel_001",
            "https://twitch.tv.example.com/example_channel_001",
            "https://twitch.tv@example.com/example_channel_001",
            "twitch.tv/example_channel_001",
            "//twitch.tv/example_channel_001",
            "ftp://twitch.tv/example_channel_001",
            "not a URL",
        ] {
            assert_eq!(channel_id_from_url(url), None, "{url}");
        }
    }

    /// M2: strict refetch reads "not live" only from a returned user.
    #[test]
    fn strict_twitch_status_needs_a_returned_user() {
        let offline = json!({"data": {"user": {"stream": null}}});
        assert!(!parse_twitch_stream_status(&offline, true).unwrap().0);
        let live = json!({"data": {"user": {"stream": {"id": "1", "type": "live", "title": "t"}}}});
        assert!(parse_twitch_stream_status(&live, true).unwrap().0);
        for bad in [
            json!({"data": {"user": null}}),
            json!({"data": {}}),
            json!({"data": {"user": {"other": 1}}}),
            json!({"data": {"user": {"stream": {}}}}),
            json!({"data": {"user": {"stream": {"type": null}}}}),
            json!({"data": {"user": {"stream": {"type": 1}}}}),
            json!({"data": {"user": {"stream": {"type": " "}}}}),
        ] {
            assert!(parse_twitch_stream_status(&bad, true).is_err(), "{bad}");
            // The lenient callers are unchanged.
            assert!(!parse_twitch_stream_status(&bad, false).unwrap().0);
        }
    }

    #[test]
    fn twitch_live_with_missing_or_same_id_keeps_a_warning_until_a_new_stream_id() {
        use super::super::danmaku::warning::{BroadcastSource, SourceObservation, WarningTracker};
        let tracker = std::sync::Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("TW", "示例频道 001", "SomeLogin", Some("stream-1"));
        let _owner = tracker.own_source(1, source.clone());
        tracker
            .mark_event(&mut tracker.capture_event(), None)
            .unwrap();
        let before = tracker.snapshot();
        for id in [
            serde_json::Value::Null,
            json!(""),
            json!("stream-1"),
            json!("stream-2"),
        ] {
            let payload = json!({"data": {"user": {"stream": {"type": "live", "id": id, "title": "changed title"}}}});
            let (is_live, _, _, observed_id) = parse_twitch_stream_status(&payload, true).unwrap();
            let observation = SourceObservation::new(
                BroadcastSource::new("TW", "示例频道 001", "somelogin", observed_id.as_deref()),
                is_live,
                false,
            );
            let released = tracker.observe_sources(&before, &[observation]);
            assert_eq!(released.is_some(), id == json!("stream-2"));
        }
        assert!(!tracker.snapshot().has_hold());
    }
}
