//! Resolve user-supplied channel identities without requiring a Data API key.
use regex::Regex;
use std::sync::LazyLock;
use std::time::Duration;

pub(crate) fn is_channel_id(value: &str) -> bool {
    value.len() == 24
        && value.starts_with("UC")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn channel_page(input: &str) -> Result<String, String> {
    let input = input.trim();
    let candidate = if input.starts_with('@') {
        format!("https://www.youtube.com/{input}")
    } else if input.starts_with("youtube.com/") || input.starts_with("www.youtube.com/") {
        format!("https://{input}")
    } else {
        input.to_string()
    };
    let url = reqwest::Url::parse(&candidate)
        .map_err(|_| "请输入 UC 频道 ID、@handle 或 YouTube 频道主页".to_string())?;
    if !matches!(url.scheme(), "https" | "http")
        || !matches!(
            url.host_str(),
            Some("youtube.com" | "www.youtube.com" | "m.youtube.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err("仅支持 YouTube 官方频道主页".into());
    }
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let path = match parts.as_slice() {
        [handle, ..] if handle.starts_with('@') && handle.len() > 1 => format!("/{handle}"),
        ["channel", id, ..] if is_channel_id(id) => format!("/channel/{id}"),
        [kind @ ("c" | "user"), name, ..] if !name.is_empty() => format!("/{kind}/{name}"),
        _ => return Err("请粘贴频道主页，不是视频、播放列表或短链接".into()),
    };
    // Build a fresh canonical URL; never fetch user query/redirect parameters.
    Ok(format!("https://www.youtube.com{path}?hl=en"))
}

fn id_from_channel_page(html: &str) -> Option<String> {
    static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
        r#"(?s)"channelMetadataRenderer"\s*:\s*\{.{0,4096}?"externalId"\s*:\s*"(UC[A-Za-z0-9_-]{22})""#,
        r#"<meta\s+itemprop="channelId"\s+content="(UC[A-Za-z0-9_-]{22})""#,
        r#"<link\s+rel="canonical"\s+href="https://www.youtube.com/channel/(UC[A-Za-z0-9_-]{22})""#,
    ].iter().map(|pattern| Regex::new(pattern).expect("channel metadata regex")).collect()
    });
    PATTERNS.iter().find_map(|pattern| {
        pattern
            .captures(html)
            .map(|captures| captures[1].to_string())
    })
}

pub(crate) async fn resolve_channel_id(input: &str, proxy: Option<&str>) -> Result<String, String> {
    let input = input.trim();
    if is_channel_id(input) {
        return Ok(input.into());
    }
    let url = channel_page(input)?;
    let parsed = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
    if let Some(id) = parsed
        .path()
        .strip_prefix("/channel/")
        .filter(|id| is_channel_id(id))
    {
        return Ok(id.into());
    }
    let response = super::http::pooled_client(proxy)
        .map_err(|e| e.to_string())?
        .get(url)
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "YouTube 频道识别失败 (HTTP {})，请稍后重试",
            response.status()
        ));
    }
    let bytes = super::http::response_bytes_limited(response, 4 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    id_from_channel_page(&String::from_utf8_lossy(&bytes))
        .ok_or_else(|| "未找到频道的 UC ID，可能遇到地区/登录验证；请重试或填写 UC ID".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_canonical_and_video_or_foreign_urls_are_rejected() {
        assert_eq!(
            channel_page("https://www.youtube.com/@hinanotachiba7/videos?x=1").unwrap(),
            "https://www.youtube.com/@hinanotachiba7?hl=en"
        );
        assert_eq!(
            channel_page("@example").unwrap(),
            "https://www.youtube.com/@example?hl=en"
        );
        for input in [
            "https://youtube.com.evil/@x",
            "https://youtube.com/watch?v=x",
            "https://user@youtube.com/@x",
            "https://youtu.be/x",
        ] {
            assert!(channel_page(input).is_err());
        }
    }
    #[test]
    fn related_channels_do_not_override_page_metadata() {
        let html = r#"{"channelId":"UCaaaaaaaaaaaaaaaaaaaaaa","metadata":{"channelMetadataRenderer":{"title":"Example","externalId":"UCbbbbbbbbbbbbbbbbbbbbbb"}}}"#;
        assert_eq!(
            id_from_channel_page(html).as_deref(),
            Some("UCbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            id_from_channel_page(r#"{"channelId":"UCaaaaaaaaaaaaaaaaaaaaaa"}"#),
            None
        );
    }
    #[tokio::test]
    async fn uc_ids_and_canonical_channel_urls_need_no_network() {
        let id = "UCabcdefghijklmnopqrstuv";
        assert_eq!(
            resolve_channel_id(id, Some("://invalid")).await.unwrap(),
            id
        );
        assert_eq!(
            resolve_channel_id(
                &format!("https://youtube.com/channel/{id}"),
                Some("://invalid")
            )
            .await
            .unwrap(),
            id
        );
    }
}
