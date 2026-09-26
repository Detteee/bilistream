use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::config::Config;

use super::banned_keywords::streaming_banned_hit;
use super::twitch::{get_twitch_status, Twitch};
use super::youtube::{get_youtube_channel_metadata, get_youtube_status_ytdlp};

/// How long the URL confirmed at switch time may be reused by the main loop.
///
/// yt-dlp is expensive and YouTube HLS URLs stay valid for minutes, so the
/// restream starts with the URL we just proved playable instead of fetching
/// again. After this window the main loop asks yt-dlp/streamlink itself.
const PREFETCH_TTL: Duration = Duration::from_secs(120);

struct PrefetchedSlot {
    config: Config,
    value: Option<(Instant, PrefetchedPlayableStream)>,
}

static PREFETCHED_PLAYABLE_STREAM: Mutex<Option<PrefetchedSlot>> = Mutex::new(None);

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
    pub m3u8_url: Option<String>,
    pub stream_id: Option<String>,
}

/// A playable URL already confirmed by yt-dlp/streamlink for a priority switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefetchedPlayableStream {
    pub platform: PriorityChannelPlatform,
    pub channel_id: String,
    pub topic: Option<String>,
    pub title: Option<String>,
    pub m3u8_url: String,
    pub stream_id: Option<String>,
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

    /// The URL yt-dlp/streamlink already confirmed, ready for the main loop to
    /// start ffmpeg without fetching again.
    pub fn prefetched_stream(
        &self,
        youtube_channel_id: &str,
        twitch_channel_id: &str,
    ) -> Option<PrefetchedPlayableStream> {
        let platform = self.platform?;
        let m3u8_url = self
            .m3u8_url
            .as_deref()
            .filter(|url| !url.is_empty())?
            .to_string();
        let channel_id = match platform {
            PriorityChannelPlatform::Youtube => youtube_channel_id,
            PriorityChannelPlatform::Twitch => twitch_channel_id,
        };
        if channel_id.is_empty() {
            return None;
        }
        Some(PrefetchedPlayableStream {
            platform,
            channel_id: channel_id.to_string(),
            topic: self.topic.clone(),
            title: self.title.clone(),
            m3u8_url,
            stream_id: self.stream_id.clone(),
        })
    }
}

fn prefetched_slot() -> MutexGuard<'static, Option<PrefetchedSlot>> {
    PREFETCHED_PLAYABLE_STREAM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn clear_prefetched_playable_stream() {
    *prefetched_slot() = None;
}

/// Park a confirmed URL for the restream that is about to restart.
pub fn store_prefetched_playable_stream(stream: PrefetchedPlayableStream, config: &Config) {
    *prefetched_slot() = Some(PrefetchedSlot {
        config: config.clone(),
        value: Some((Instant::now(), stream)),
    });
}

/// Take the parked URL if it is for this platform and channel and still fresh.
pub fn take_prefetched_playable_stream(
    platform: PriorityChannelPlatform,
    channel_id: &str,
) -> Option<PrefetchedPlayableStream> {
    let mut slot = prefetched_slot();
    if slot
        .as_ref()
        .is_some_and(|slot| !crate::config::config_is_current(&slot.config))
    {
        *slot = None;
    }
    let slot = slot.as_mut()?;
    take_matching_prefetch(
        &mut slot.value,
        platform,
        channel_id,
        Instant::now(),
        PREFETCH_TTL,
    )
}

fn take_matching_prefetch(
    slot: &mut Option<(Instant, PrefetchedPlayableStream)>,
    platform: PriorityChannelPlatform,
    channel_id: &str,
    now: Instant,
    ttl: Duration,
) -> Option<PrefetchedPlayableStream> {
    let (stored_at, stream) = slot.as_ref()?;
    if now.saturating_duration_since(*stored_at) > ttl {
        *slot = None;
        return None;
    }
    if stream.platform != platform || stream.channel_id != channel_id {
        return None;
    }
    slot.take().map(|(_, stream)| stream)
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
                    ..Default::default()
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
                    ..Default::default()
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
    stream_id: Option<String>,
) -> Option<PriorityChannelLiveness> {
    if !is_live {
        return None;
    }

    let Some(m3u8_url) = m3u8_url.filter(|url| !url.is_empty()) else {
        tracing::info!(
            "优先频道监控: {} 显示开播但拿不到可播放的流地址，暂不切换",
            platform.label()
        );
        return None;
    };

    Some(PriorityChannelLiveness {
        platform: Some(platform),
        title: title.filter(|title| !title.is_empty()),
        topic: topic.filter(|topic| !topic.is_empty()),
        m3u8_url: Some(m3u8_url),
        stream_id: stream_id.filter(|id| !id.is_empty()),
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
/// only happens once a switch is actually on the table. The confirmed URL is
/// handed to the main loop via [`store_prefetched_playable_stream`] so the
/// restream can start without a second fetch.
///
/// A platform that errors, or that reports live without a URL, is treated as not
/// switchable, and the next platform still gets its turn.
pub async fn resolve_playable_priority_channel(cfg: &Config) -> PriorityChannelLiveness {
    if !cfg.priority_channel.youtube_channel_id.is_empty() {
        match get_youtube_status_ytdlp(&cfg.priority_channel.youtube_channel_id).await {
            Ok((is_live, topic, title, m3u8_url, _, stream_id)) => {
                if let Some(liveness) = playable_liveness(
                    PriorityChannelPlatform::Youtube,
                    is_live,
                    topic,
                    title,
                    m3u8_url,
                    stream_id,
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
                Ok((is_live, topic, title, m3u8_url, _, stream_id)) => {
                    if let Some(liveness) = playable_liveness(
                        PriorityChannelPlatform::Twitch,
                        is_live,
                        topic,
                        title,
                        m3u8_url,
                        stream_id,
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
    fn a_platform_reporting_offline_is_not_switchable() {
        assert_eq!(
            playable_liveness(
                PriorityChannelPlatform::Youtube,
                false,
                None,
                Some("stream".to_string()),
                Some("https://example.com/live.m3u8".to_string()),
                None,
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
                None,
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
            Some("stream-id".to_string()),
        )
        .expect("a live platform with a stream URL is switchable");

        assert!(liveness.is_live());
        assert_eq!(liveness.platform, Some(PriorityChannelPlatform::Twitch));
        assert_eq!(liveness.title.as_deref(), Some("stream"));
        assert_eq!(liveness.topic.as_deref(), Some("Just Chatting"));
        assert_eq!(
            liveness.m3u8_url.as_deref(),
            Some("https://example.com/live.m3u8")
        );
        assert_eq!(liveness.stream_id.as_deref(), Some("stream-id"));
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
            None,
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
            None,
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
            None,
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
            None,
        )
        .expect("a live platform with a stream URL is switchable");

        let keywords = vec!["asmr".to_string(), "just chatting".to_string()];
        assert!(liveness.banned_streaming_keyword(&keywords).is_none());
    }
}
