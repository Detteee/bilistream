use crate::config::Config;

use super::banned_keywords::streaming_banned_hit;
use super::twitch::{get_twitch_status, Twitch};
use super::youtube::{get_youtube_channel_metadata, get_youtube_status};

/// The platform the priority channel turned out to be live on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityChannelPlatform {
    Youtube,
    Twitch,
}

impl PriorityChannelPlatform {
    /// Display name used in logs and danmaku.
    pub fn label(self) -> &'static str {
        match self {
            PriorityChannelPlatform::Youtube => "YouTube",
            PriorityChannelPlatform::Twitch => "Twitch",
        }
    }

    /// Identifier the WebUI expects in status payloads.
    pub fn api_name(self) -> &'static str {
        match self {
            PriorityChannelPlatform::Youtube => "youtube",
            PriorityChannelPlatform::Twitch => "twitch",
        }
    }
}

/// Where the priority channel is live, if anywhere.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriorityChannelLiveness {
    pub platform: Option<PriorityChannelPlatform>,
    pub title: Option<String>,
    pub topic: Option<String>,
}

impl PriorityChannelLiveness {
    pub fn is_live(&self) -> bool {
        self.platform.is_some()
    }

    /// Keyword from `streaming_banned_keywords` that would make the monitor skip
    /// this stream. Auto-switch must not force a restream onto a title the main
    /// loop would discard.
    pub fn banned_streaming_keyword(&self, keywords: &[String]) -> Option<String> {
        streaming_banned_hit(self.topic.as_deref(), self.title.as_deref(), keywords)
    }
}

/// Resolve whether the configured priority channel is live, preferring YouTube.
///
/// Only metadata is fetched: neither yt-dlp nor streamlink is asked for a
/// playable URL. That keeps the WebUI's frequent status polling cheap, but it
/// also means the answer is only as fresh as the upstream APIs — see
/// [`resolve_playable_priority_channel`], which the auto-switch decision uses
/// instead.
///
/// A platform that errors is reported as offline, matching what both callers
/// did before.
pub async fn resolve_priority_channel_liveness(cfg: &Config) -> PriorityChannelLiveness {
    if !cfg.priority_channel.youtube_channel_id.is_empty() {
        match get_youtube_channel_metadata(&cfg.priority_channel.youtube_channel_id).await {
            Ok(status) if status.is_live => {
                return PriorityChannelLiveness {
                    platform: Some(PriorityChannelPlatform::Youtube),
                    title: status.title,
                    topic: status.topic.filter(|topic| !topic.is_empty()),
                };
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("优先频道监控: YouTube 状态检查失败: {}", e),
        }
    }

    if !cfg.priority_channel.twitch_channel_id.is_empty() {
        match get_twitch_status(&cfg.priority_channel.twitch_channel_id).await {
            Ok((true, topic, title, _)) => {
                return PriorityChannelLiveness {
                    platform: Some(PriorityChannelPlatform::Twitch),
                    title,
                    topic: topic.filter(|topic| !topic.is_empty()),
                };
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("优先频道监控: Twitch 状态检查失败: {}", e),
        }
    }

    PriorityChannelLiveness::default()
}

/// A platform is worth switching to only when it reports live *and* hands over a
/// playable URL.
fn playable_liveness(
    platform: PriorityChannelPlatform,
    is_live: bool,
    topic: Option<String>,
    title: Option<String>,
    m3u8_url: Option<String>,
) -> Option<PriorityChannelLiveness> {
    if !is_live {
        return None;
    }

    if m3u8_url.is_none_or(|url| url.is_empty()) {
        tracing::info!(
            "优先频道监控: {} 显示开播但拿不到可播放的流地址，暂不切换",
            platform.label()
        );
        return None;
    }

    Some(PriorityChannelLiveness {
        platform: Some(platform),
        title: title.filter(|title| !title.is_empty()),
        topic: topic.filter(|topic| !topic.is_empty()),
    })
}

/// Resolve the priority channel to a stream ffmpeg can actually play right now.
///
/// [`resolve_priority_channel_liveness`] answers from metadata alone, and that
/// lags reality in both directions: Holodex flips a scheduled stream to live at
/// its scheduled time even while the streamer is still holding off, and keeps a
/// finished stream marked live for a while after it ends. Switching on metadata
/// alone therefore tears down a working restream for a channel with nothing to
/// play, and bounces back to a priority channel that just went off air.
///
/// yt-dlp/streamlink do not lag, so the auto-switch decision goes through this
/// instead. Metadata is still the first gate — the extra yt-dlp/streamlink call
/// only happens once a switch is actually on the table. The resolved URL is
/// discarded: the main loop resolves its own after the restart, and this one
/// would be stale by then anyway.
///
/// A platform that errors, or that reports live without a URL, is treated as not
/// switchable, and the next platform still gets its turn.
pub async fn resolve_playable_priority_channel(cfg: &Config) -> PriorityChannelLiveness {
    if !cfg.priority_channel.youtube_channel_id.is_empty() {
        match get_youtube_status(&cfg.priority_channel.youtube_channel_id).await {
            Ok((is_live, topic, title, m3u8_url, _, _)) => {
                if let Some(liveness) = playable_liveness(
                    PriorityChannelPlatform::Youtube,
                    is_live,
                    topic,
                    title,
                    m3u8_url,
                ) {
                    return liveness;
                }
            }
            Err(e) => tracing::warn!("优先频道监控: YouTube 流地址获取失败: {}", e),
        }
    }

    if !cfg.priority_channel.twitch_channel_id.is_empty() {
        match Twitch::new(
            &cfg.priority_channel.twitch_channel_id,
            cfg.twitch.proxy_region.clone(),
            cfg.twitch.proxy.clone(),
        ) {
            Ok(client) => match client.get_status().await {
                Ok((is_live, topic, title, m3u8_url, _, _)) => {
                    if let Some(liveness) = playable_liveness(
                        PriorityChannelPlatform::Twitch,
                        is_live,
                        topic,
                        title,
                        m3u8_url,
                    ) {
                        return liveness;
                    }
                }
                Err(e) => tracing::warn!("优先频道监控: Twitch 流地址获取失败: {}", e),
            },
            Err(e) => tracing::warn!("优先频道监控: Twitch 客户端初始化失败: {}", e),
        }
    }

    PriorityChannelLiveness::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_defaults_to_offline() {
        let liveness = PriorityChannelLiveness::default();

        assert!(!liveness.is_live());
        assert_eq!(liveness.platform, None);
        assert_eq!(liveness.title, None);
        assert_eq!(liveness.topic, None);
    }

    #[test]
    fn platform_labels_match_log_and_api_spellings() {
        assert_eq!(PriorityChannelPlatform::Youtube.label(), "YouTube");
        assert_eq!(PriorityChannelPlatform::Youtube.api_name(), "youtube");
        assert_eq!(PriorityChannelPlatform::Twitch.label(), "Twitch");
        assert_eq!(PriorityChannelPlatform::Twitch.api_name(), "twitch");
    }

    #[test]
    fn liveness_reports_live_once_a_platform_is_set() {
        let liveness = PriorityChannelLiveness {
            platform: Some(PriorityChannelPlatform::Twitch),
            title: Some("stream".to_string()),
            topic: None,
        };

        assert!(liveness.is_live());
    }

    #[test]
    fn a_platform_reporting_offline_is_not_switchable() {
        assert_eq!(
            playable_liveness(
                PriorityChannelPlatform::Youtube,
                false,
                None,
                Some("stream".to_string()),
                Some("https://example.com/live.m3u8".to_string()),
            ),
            None
        );
    }

    #[test]
    fn live_metadata_without_a_stream_url_is_not_switchable() {
        // Holodex marks a scheduled stream live at its scheduled time, and keeps
        // an ended one live for a while: without a URL there is nothing to play.
        assert_eq!(
            playable_liveness(
                PriorityChannelPlatform::Youtube,
                true,
                None,
                Some("stream".to_string()),
                None,
            ),
            None
        );
        assert_eq!(
            playable_liveness(
                PriorityChannelPlatform::Twitch,
                true,
                None,
                Some("stream".to_string()),
                Some(String::new()),
            ),
            None
        );
    }

    #[test]
    fn live_metadata_with_a_stream_url_switches() {
        let liveness = playable_liveness(
            PriorityChannelPlatform::Twitch,
            true,
            Some("Just Chatting".to_string()),
            Some("stream".to_string()),
            Some("https://example.com/live.m3u8".to_string()),
        )
        .expect("a live platform with a stream URL is switchable");

        assert!(liveness.is_live());
        assert_eq!(liveness.platform, Some(PriorityChannelPlatform::Twitch));
        assert_eq!(liveness.title.as_deref(), Some("stream"));
        assert_eq!(liveness.topic.as_deref(), Some("Just Chatting"));
    }

    #[test]
    fn an_empty_title_is_reported_as_missing() {
        // Twitch fills a missing title in with an empty string; callers log it
        // as "无标题" only when it is None.
        let liveness = playable_liveness(
            PriorityChannelPlatform::Twitch,
            true,
            Some(String::new()),
            Some(String::new()),
            Some("https://example.com/live.m3u8".to_string()),
        )
        .expect("a live platform with a stream URL is switchable");

        assert_eq!(liveness.title, None);
        assert_eq!(liveness.topic, None);
    }

    #[test]
    fn a_banned_streaming_keyword_blocks_auto_switch() {
        let liveness = playable_liveness(
            PriorityChannelPlatform::Youtube,
            true,
            None,
            Some("【ASMR】睡眠導入".to_string()),
            Some("https://example.com/live.m3u8".to_string()),
        )
        .expect("a live platform with a stream URL is switchable");

        let keywords = vec!["asmr".to_string(), "gta".to_string()];
        assert_eq!(
            liveness.banned_streaming_keyword(&keywords).as_deref(),
            Some("asmr")
        );
    }

    #[test]
    fn a_banned_keyword_in_the_topic_blocks_auto_switch() {
        let liveness = playable_liveness(
            PriorityChannelPlatform::Twitch,
            true,
            Some("Just Chatting".to_string()),
            Some("雑談".to_string()),
            Some("https://example.com/live.m3u8".to_string()),
        )
        .expect("a live platform with a stream URL is switchable");

        let keywords = vec!["just chatting".to_string()];
        assert_eq!(
            liveness.banned_streaming_keyword(&keywords).as_deref(),
            Some("just chatting")
        );
    }

    #[test]
    fn a_clean_title_does_not_block_auto_switch() {
        let liveness = playable_liveness(
            PriorityChannelPlatform::Youtube,
            true,
            Some("League of Legends".to_string()),
            Some("ランク".to_string()),
            Some("https://example.com/live.m3u8".to_string()),
        )
        .expect("a live platform with a stream URL is switchable");

        let keywords = vec!["asmr".to_string(), "just chatting".to_string()];
        assert!(liveness.banned_streaming_keyword(&keywords).is_none());
    }
}
