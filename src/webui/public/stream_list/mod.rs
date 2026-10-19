//! Public stream lists built from shared raw items; viewers read only snapshots.
mod images;
mod payload;
mod snapshot;

pub use snapshot::remap_after_areas_change;
pub(super) use snapshot::{current_public_streams, start_streams_watch};

#[cfg(test)]
mod test_support {
    use super::payload::{build_public_streams, PublicStream};
    use crate::config::{Channel, ChannelPlatforms};
    use crate::plugins::stream_item::{StreamChannel, StreamItem};
    pub(super) fn channel(
        name: &str,
        aliases: &[&str],
        youtube: Option<&str>,
        twitch: Option<&str>,
    ) -> Channel {
        Channel {
            name: name.to_string(),
            niconico_name: None,
            aliases: aliases.iter().map(|alias| alias.to_string()).collect(),
            platforms: ChannelPlatforms {
                youtube: youtube.map(str::to_string),
                twitch: twitch.map(str::to_string),
                niconico: None,
            },
            riot_puuid: None,
        }
    }

    pub(super) fn stream(title: &str, topic: Option<&str>, channel_id: &str) -> StreamItem {
        stream_on("vid1", title, topic, channel_id, "live", None)
    }

    pub(super) fn stream_on(
        id: &str,
        title: &str,
        topic: Option<&str>,
        channel_id: &str,
        status: &str,
        scheduled: Option<&str>,
    ) -> StreamItem {
        StreamItem {
            id: id.to_string(),
            title: title.to_string(),
            stream_type: "stream".to_string(),
            topic_id: topic.map(str::to_string),
            published_at: None,
            available_at: None,
            status: status.to_string(),
            start_scheduled: scheduled.map(str::to_string),
            start_actual: None,
            live_viewers: Some(1200),
            channel: StreamChannel {
                id: channel_id.to_string(),
                name: "Kamito".to_string(),
                photo: None,
            },
            link: None,
            thumbnail: Some(format!("https://i.ytimg.com/vi/{id}/maxres.jpg")),
            placeholder_type: None,
            yt_confirmed: false,
        }
    }

    pub(super) fn kamito() -> Vec<Channel> {
        vec![channel(
            "Kamito",
            &["kmt"],
            Some("UCkamito"),
            Some("kamito_jp"),
        )]
    }

    pub(super) fn build(
        streams: Vec<StreamItem>,
        enabled: bool,
        banned: &[&str],
    ) -> Vec<PublicStream> {
        let banned: Vec<String> = banned.iter().map(|k| k.to_string()).collect();
        build_public_streams(streams, &kamito(), enabled, &banned)
    }
}
