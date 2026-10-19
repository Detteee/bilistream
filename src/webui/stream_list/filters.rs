//! Roster membership, stale schedules, and per-platform live slots.
use crate::plugins::twitch::channel_id_from_url;
use std::collections::HashSet;

/// Holodex Favorites/Home drop streams that never got `start_actual` once the
/// schedule is more than two hours old (`!start_actual && now > scheduled + 2h`).
/// Rows YouTube classified skip this: its `actualStartTime` is authoritative.
fn holodex_has_start_actual(stream: &crate::plugins::stream_item::StreamItem) -> bool {
    stream
        .start_actual
        .as_deref()
        .is_some_and(|start| !start.is_empty())
}

fn holodex_scheduled_at(
    stream: &crate::plugins::stream_item::StreamItem,
) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(stream.start_scheduled.as_deref()?)
        .ok()
        .map(|scheduled| scheduled.with_timezone(&chrono::Utc))
}

fn holodex_unconfirmed_and_stale(
    stream: &crate::plugins::stream_item::StreamItem,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if stream.yt_confirmed || holodex_has_start_actual(stream) {
        return false;
    }
    holodex_scheduled_at(stream)
        .is_some_and(|scheduled| now > scheduled + chrono::Duration::hours(2))
}

pub(crate) fn filter_streams(
    streams: Vec<crate::plugins::stream_item::StreamItem>,
    allowed_channel_ids: HashSet<String>,
) -> Vec<crate::plugins::stream_item::StreamItem> {
    filter_streams_at(streams, allowed_channel_ids, chrono::Utc::now())
}

/// Roster membership and Holodex avatars use the YouTube channel id, including
/// Niconico/Twitch placeholders. Upcoming items are hidden only when *that
/// platform* is already live, so 激ロー and the official YouTube relay both
/// stay on the panel.
fn live_slot_key(stream: &crate::plugins::stream_item::StreamItem) -> String {
    if stream
        .link
        .as_deref()
        .and_then(crate::plugins::live_id_from_link)
        .is_some()
    {
        return format!("NC:{}", stream.channel.id);
    }
    if let Some(login) = stream.link.as_deref().and_then(channel_id_from_url) {
        return format!("TW:{login}");
    }
    format!("YT:{}", stream.channel.id)
}

fn filter_streams_at(
    streams: Vec<crate::plugins::stream_item::StreamItem>,
    allowed_channel_ids: HashSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<crate::plugins::stream_item::StreamItem> {
    let mut live_slots: HashSet<String> = HashSet::new();
    for stream in &streams {
        if stream.status == "live" && !holodex_unconfirmed_and_stale(stream, now) {
            live_slots.insert(live_slot_key(stream));
        }
    }

    let thirty_hours_later = now + chrono::Duration::hours(30);

    streams
        .into_iter()
        .filter(|stream| {
            if !allowed_channel_ids.contains(&stream.channel.id) {
                return false;
            }

            if holodex_unconfirmed_and_stale(stream, now) {
                return false;
            }

            if stream.status == "live" {
                return true;
            }

            if live_slots.contains(&live_slot_key(stream)) {
                return false;
            }

            if stream.status == "upcoming" {
                if let Some(scheduled_utc) = holodex_scheduled_at(stream) {
                    return scheduled_utc <= thirty_hours_later;
                }
                return true;
            }

            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::stream_item::{StreamChannel, StreamItem};
    use chrono::{DateTime, Utc};
    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn stream(
        id: &str,
        status: &str,
        scheduled: Option<&str>,
        start_actual: Option<&str>,
    ) -> StreamItem {
        StreamItem {
            id: id.to_string(),
            title: format!("{id} title"),
            stream_type: "stream".to_string(),
            topic_id: None,
            published_at: None,
            available_at: scheduled.map(str::to_string),
            status: status.to_string(),
            start_scheduled: scheduled.map(str::to_string),
            start_actual: start_actual.map(str::to_string),
            live_viewers: None,
            channel: StreamChannel {
                id: "channel-id".to_string(),
                ..Default::default()
            },
            link: None,
            thumbnail: None,
            placeholder_type: None,
            yt_confirmed: false,
        }
    }

    fn ids() -> HashSet<String> {
        HashSet::from(["channel-id".to_string()])
    }

    fn kept(streams: Vec<StreamItem>, now: DateTime<Utc>) -> Vec<String> {
        filter_streams_at(streams, ids(), now)
            .into_iter()
            .map(|stream| stream.id)
            .collect()
    }

    #[test]
    fn a_live_stream_with_start_actual_is_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![stream(
            "confirmed",
            "live",
            Some("2026-09-01T14:00:00Z"),
            Some("2026-09-01T14:03:00Z"),
        )];
        assert_eq!(kept(streams, now), vec!["confirmed"]);
    }

    #[test]
    fn a_live_stream_without_start_actual_is_kept_inside_the_two_hour_grace() {
        let now = at("2026-09-01T15:30:00Z");
        let streams = vec![stream("fresh", "live", Some("2026-09-01T14:00:00Z"), None)];
        assert_eq!(kept(streams, now), vec!["fresh"]);
    }

    #[test]
    fn a_live_stream_without_start_actual_is_kept_at_exactly_two_hours() {
        let now = at("2026-09-01T16:00:00Z");
        let streams = vec![stream("edge", "live", Some("2026-09-01T14:00:00Z"), None)];
        assert_eq!(kept(streams, now), vec!["edge"]);
    }

    #[test]
    fn an_empty_start_actual_counts_as_missing() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![stream(
            "empty",
            "live",
            Some("2026-09-01T14:00:00Z"),
            Some(""),
        )];
        assert!(kept(streams, now).is_empty());
    }

    #[test]
    fn a_live_stream_without_a_schedule_is_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![stream("no-sched", "live", None, None)];
        assert_eq!(kept(streams, now), vec!["no-sched"]);
    }

    #[test]
    fn a_stale_unconfirmed_upcoming_is_dropped() {
        let now = at("2026-09-01T17:00:00Z");
        let streams = vec![stream(
            "waiting-room",
            "upcoming",
            Some("2026-09-01T14:00:00Z"),
            None,
        )];
        assert!(kept(streams, now).is_empty());
    }

    #[test]
    fn a_hung_live_does_not_hide_a_later_upcoming_on_the_same_channel() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            stream("hung", "live", Some("2026-09-01T14:00:00Z"), None),
            stream("later", "upcoming", Some("2026-09-02T20:00:00Z"), None),
        ];
        assert_eq!(kept(streams, now), vec!["later"]);
    }

    #[test]
    fn a_youtube_confirmed_upcoming_past_its_schedule_is_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let mut late = stream("late", "upcoming", Some("2026-09-01T14:00:00Z"), None);
        late.yt_confirmed = true;
        assert_eq!(kept(vec![late], now), vec!["late"]);
    }

    fn niconico_live(id: &str) -> StreamItem {
        let mut stream = stream(
            id,
            "live",
            Some("2026-09-02T14:00:00Z"),
            Some("2026-09-02T14:00:00Z"),
        );
        stream.stream_type = "placeholder".to_string();
        stream.link = Some("https://live.nicovideo.jp/watch/lv351182284".to_string());
        stream
    }

    fn niconico_upcoming(id: &str) -> StreamItem {
        let mut stream = stream(id, "upcoming", Some("2026-09-02T20:00:00Z"), None);
        stream.stream_type = "placeholder".to_string();
        stream.link = Some("https://live.nicovideo.jp/watch/lv351230205".to_string());
        stream
    }

    #[test]
    fn a_niconico_live_and_the_same_channel_s_youtube_live_are_both_kept() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            niconico_live("niconico-lv1"),
            stream(
                "yt-live",
                "live",
                Some("2026-09-02T14:05:00Z"),
                Some("2026-09-02T14:05:00Z"),
            ),
        ];
        assert_eq!(kept(streams, now), vec!["niconico-lv1", "yt-live"]);
    }

    #[test]
    fn a_niconico_live_does_not_hide_the_same_channel_s_youtube_upcoming() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            niconico_live("niconico-lv1"),
            stream("yt-up", "upcoming", Some("2026-09-02T20:00:00Z"), None),
        ];
        assert_eq!(kept(streams, now), vec!["niconico-lv1", "yt-up"]);
    }

    #[test]
    fn a_youtube_live_does_not_hide_the_same_channel_s_niconico_upcoming() {
        let now = at("2026-09-02T15:00:00Z");
        let streams = vec![
            stream(
                "yt-live",
                "live",
                Some("2026-09-02T14:05:00Z"),
                Some("2026-09-02T14:05:00Z"),
            ),
            niconico_upcoming("niconico-lv2"),
        ];
        assert_eq!(kept(streams, now), vec!["yt-live", "niconico-lv2"]);
    }

    #[test]
    fn mixed_case_twitch_urls_share_a_live_slot_without_hiding_other_platforms() {
        let now = at("2026-09-02T15:00:00Z");
        let mut live = stream("tw-live", "live", None, Some("2026-09-02T14:00:00Z"));
        live.stream_type = "placeholder".to_string();
        live.link = Some(" HTTPS://WWW.TWITCH.TV/Example_Channel_001?x=1#live ".to_string());
        let mut upcoming = stream("tw-next", "upcoming", Some("2026-09-02T20:00:00Z"), None);
        upcoming.stream_type = "placeholder".to_string();
        upcoming.link = Some("http://twitch.tv/example_channel_001/videos".to_string());
        assert_eq!(
            kept(
                vec![
                    live,
                    upcoming,
                    stream("yt-next", "upcoming", None, None),
                    niconico_upcoming("nc-next")
                ],
                now
            ),
            vec!["tw-live", "yt-next", "nc-next"]
        );
    }

    #[test]
    fn an_embedded_twitch_hostname_does_not_take_a_twitch_live_slot() {
        let mut item = stream("external", "live", None, None);
        item.link = Some("https://example.com/twitch.tv/example_channel_001".to_string());
        assert_eq!(live_slot_key(&item), "YT:channel-id");
    }
}
