use crate::config::Config;

use super::twitch::get_twitch_status;
use super::youtube::get_youtube_channel_metadata;

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
}

impl PriorityChannelLiveness {
    pub fn is_live(&self) -> bool {
        self.platform.is_some()
    }
}

/// Resolve whether the configured priority channel is live, preferring YouTube.
///
/// The background monitor and the WebUI status refresh both go through this, so
/// the panel and the switching decision always agree. Only metadata is fetched:
/// neither yt-dlp nor streamlink is asked for a playable URL, which the callers
/// discarded anyway.
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
                };
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("优先频道监控: YouTube 状态检查失败: {}", e),
        }
    }

    if !cfg.priority_channel.twitch_channel_id.is_empty() {
        match get_twitch_status(&cfg.priority_channel.twitch_channel_id).await {
            Ok((true, _, title, _)) => {
                return PriorityChannelLiveness {
                    platform: Some(PriorityChannelPlatform::Twitch),
                    title,
                };
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("优先频道监控: Twitch 状态检查失败: {}", e),
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
        };

        assert!(liveness.is_live());
    }
}
