//! Holodex stream list for the public page.
//!
//! Refreshed on a timer here rather than on request: viewers only ever read a
//! snapshot, so upstream cost is fixed at one call per interval no matter how
//! many people have the page open, and there is no refresh button to hammer.
//!
//! Only channels listed in channels.json are fetched, and the JWT/favorites
//! branch of the Holodex client is never reached.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Channel, Config};
use crate::plugins::banned_keywords::{banned_keyword_hit, danmaku_banned_keywords};
use crate::plugins::holodex::HolodexStream;

/// Why a stream cannot be requested. The matched keyword itself is never sent:
/// a public page should not publish the blocklist.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotSwitchable {
    /// Danmaku commands are off, so nothing is requestable right now.
    DanmakuDisabled,
    /// Title or topic hits the danmaku banned keyword list.
    BannedKeyword,
    /// The danmaku command only understands YT and TW.
    UnsupportedPlatform,
    /// Not in channels.json, so no command name resolves to it.
    UnknownChannel,
    /// Every name and alias contains whitespace, which the command parser
    /// strips before matching, so no typed command could ever match.
    NoCommandName,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PublicStream {
    pub id: String,
    pub title: String,
    pub topic: Option<String>,
    pub status: String,
    pub is_placeholder: bool,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub live_viewers: Option<i32>,
    pub channel_name: String,
    pub channel_photo: Option<String>,
    pub thumbnail: Option<String>,
    pub link: Option<String>,
    pub suggested_area_id: Option<u64>,
    pub suggested_area_name: Option<String>,
    /// `YT` or `TW`, matching what the danmaku command expects.
    pub command_platform: Option<String>,
    /// The whitespace-free name or alias to put in the command.
    pub command_channel: Option<String>,
    pub switchable: bool,
    pub reason: Option<NotSwitchable>,
}

/// What the page needs to know about a configured channel to build a command.
struct CommandTarget {
    platform: &'static str,
    name: Option<String>,
}

/// The command parser strips whitespace before matching against channels.json,
/// so a name containing any is unusable no matter what the viewer types.
fn command_name(channel: &Channel) -> Option<String> {
    std::iter::once(&channel.name)
        .chain(channel.aliases.iter())
        .find(|candidate| {
            !candidate.trim().is_empty() && !candidate.chars().any(char::is_whitespace)
        })
        .cloned()
}

fn twitch_login_from_link(link: Option<&str>) -> Option<String> {
    let link = link?;
    let rest = link
        .split_once("twitch.tv/")
        .map(|(_, rest)| rest)?
        .trim_matches('/');
    let login = rest.split(['/', '?', '#']).next()?.trim();
    (!login.is_empty()).then(|| login.to_lowercase())
}

/// Resolves a stream back to a configured channel, and to the platform token
/// the danmaku command uses.
fn command_target(
    channels: &[Channel],
    youtube_channel_id: &str,
    link: Option<&str>,
) -> Option<CommandTarget> {
    let twitch_login = twitch_login_from_link(link);

    // A Twitch placeholder is a Twitch stream even though Holodex keys it by
    // the YouTube channel it belongs to.
    if let Some(login) = twitch_login.as_deref() {
        let matched = channels.iter().find(|channel| {
            channel
                .platforms
                .twitch
                .as_deref()
                .is_some_and(|configured| configured.eq_ignore_ascii_case(login))
        });
        return matched.map(|channel| CommandTarget {
            platform: "TW",
            name: command_name(channel),
        });
    }

    let matched = channels.iter().find(|channel| {
        channel
            .platforms
            .youtube
            .as_deref()
            .is_some_and(|configured| configured == youtube_channel_id)
    })?;

    Some(CommandTarget {
        platform: "YT",
        name: command_name(matched),
    })
}

/// Whether a stream can be requested, and why not when it cannot.
///
/// Runs the same keyword predicate the danmaku path uses, so the page cannot
/// offer a request that would be rejected on arrival.
fn switchability(
    stream: &HolodexStream,
    target: Option<&CommandTarget>,
    danmaku_enabled: bool,
    banned: &[String],
) -> Option<NotSwitchable> {
    if !danmaku_enabled {
        return Some(NotSwitchable::DanmakuDisabled);
    }

    let Some(target) = target else {
        return Some(NotSwitchable::UnknownChannel);
    };
    if target.name.is_none() {
        return Some(NotSwitchable::NoCommandName);
    }

    let haystack = crate::plugins::banned_keywords::danmaku_haystack(
        stream.topic_id.as_deref().unwrap_or_default(),
        &stream.title,
    );
    if banned_keyword_hit(&haystack, banned).is_some() {
        return Some(NotSwitchable::BannedKeyword);
    }

    None
}

/// Niconico and anything else Holodex surfaces cannot be requested, because
/// the danmaku command only parses YT and TW.
fn platform_is_supported(target: Option<&CommandTarget>) -> bool {
    matches!(target, Some(t) if t.platform == "YT" || t.platform == "TW")
}

pub fn build_public_streams(
    streams: Vec<HolodexStream>,
    channels: &[Channel],
    danmaku_enabled: bool,
    banned: &[String],
) -> Vec<PublicStream> {
    streams
        .into_iter()
        .map(|stream| {
            let target = command_target(channels, &stream.channel.id, stream.link.as_deref());
            let reason = if !platform_is_supported(target.as_ref()) && target.is_some() {
                Some(NotSwitchable::UnsupportedPlatform)
            } else {
                switchability(&stream, target.as_ref(), danmaku_enabled, banned)
            };

            let (suggested_area_id, suggested_area_name) = suggested_area(&stream);

            PublicStream {
                id: stream.id,
                title: stream.title,
                topic: stream.topic_id,
                status: stream.status,
                is_placeholder: stream.stream_type == "placeholder",
                start_scheduled: stream.start_scheduled,
                start_actual: stream.start_actual,
                live_viewers: stream.live_viewers,
                channel_name: stream.channel.name,
                channel_photo: stream.channel.photo.filter(|photo| !photo.is_empty()),
                thumbnail: stream.thumbnail,
                link: stream.link,
                suggested_area_id,
                suggested_area_name,
                command_platform: target.as_ref().map(|target| target.platform.to_string()),
                command_channel: target.and_then(|target| target.name),
                switchable: reason.is_none(),
                reason,
            }
        })
        .collect()
}

/// Mirrors the dashboard's topic-and-title area guess so the page offers the
/// same default the operator would see.
fn suggested_area(stream: &HolodexStream) -> (Option<u64>, Option<String>) {
    let title_for_detection = match stream.topic_id.as_deref() {
        Some(topic) => format!("{} {}", topic, stream.title),
        None => stream.title.clone(),
    };

    let mut area_id = 235;
    if let Some(topic) = stream.topic_id.as_deref() {
        let topic = topic.to_lowercase();
        if topic.contains("freechat") || topic.contains("talk") || topic.contains("singing") {
            area_id = 530;
        }
        if topic.contains("talk")
            || topic.contains("zatsudan")
            || topic.contains("雑談")
            || topic.contains("just chatting")
        {
            area_id = 646;
        }
    }
    if area_id == 235 {
        area_id = crate::plugins::check_area_id_with_title(&title_for_detection, 235);
    }

    if area_id == 235 {
        (None, None)
    } else {
        (Some(area_id), crate::plugins::get_area_name(area_id))
    }
}

// Snapshot -------------------------------------------------------------------

struct StreamsSnapshot {
    body: String,
    etag: String,
}

static SNAPSHOT: RwLock<Option<StreamsSnapshot>> = RwLock::new(None);
static ETAG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The cached list and its ETag, or `None` before the first successful fetch.
pub fn current_public_streams() -> Option<(String, String)> {
    let guard = SNAPSHOT.read().ok()?;
    let snapshot = guard.as_ref()?;
    Some((snapshot.body.clone(), snapshot.etag.clone()))
}

fn store_snapshot(streams: &[PublicStream]) {
    let Ok(body) = serde_json::to_string(streams) else {
        return;
    };

    if let Ok(mut guard) = SNAPSHOT.write() {
        if let Some(previous) = guard.as_ref() {
            if previous.body == body {
                *guard = Some(StreamsSnapshot {
                    body: previous.body.clone(),
                    etag: previous.etag.clone(),
                });
                return;
            }
        }
        *guard = Some(StreamsSnapshot {
            body,
            etag: format!("\"{:x}\"", ETAG_COUNTER.fetch_add(1, Ordering::Relaxed)),
        });
    }
}

/// YouTube channel ids to ask Holodex about: exactly what channels.json lists,
/// plus whatever channel is configured right now.
fn holodex_channel_ids(cfg: &Config, channels: &[Channel]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for channel in channels {
        if let Some(id) = channel.platforms.youtube.as_deref() {
            if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    let configured = cfg.youtube.channel_id.trim();
    if !configured.is_empty() && !ids.iter().any(|existing| existing == configured) {
        ids.push(configured.to_string());
    }
    ids
}

/// One refresh cycle. Returns false when the list could not be refreshed, so
/// the caller knows the snapshot it still holds is the last good one.
pub async fn refresh_public_streams() -> bool {
    let Ok(cfg) = crate::config::load_config().await else {
        return false;
    };
    let Ok(channels_data) = crate::config::load_channels() else {
        return false;
    };
    let channels = channels_data.channels;

    let ids = holodex_channel_ids(&cfg, &channels);
    if ids.is_empty() {
        return false;
    }

    // A dashboard fetch on this node counts as this interval's call, so the
    // two together stay at one upstream request per interval.
    let max_age = Duration::from_secs(cfg.cluster.public_status.holodex_refresh_secs);
    let streams = match super::holodex_cache::get_or_fetch(ids, max_age).await {
        Ok(streams) => streams,
        Err(e) => {
            tracing::debug!("公开状态页 Holodex 刷新失败: {}", e);
            return false;
        }
    };

    let banned = danmaku_banned_keywords();
    let mut public = build_public_streams(
        streams,
        &channels,
        cfg.bililive.enable_danmaku_command,
        &banned,
    );

    // Only a successful fetch reaches here, so the list is safe to reconcile
    // against; a failed poll returns early above and leaves the cache alone.
    let upstream: Vec<String> = public
        .iter()
        .filter_map(|stream| stream.thumbnail.clone())
        .collect();
    super::thumbnails::reconcile(&upstream).await;
    rewrite_thumbnails(&mut public);

    store_snapshot(&public);
    true
}

/// Points each thumbnail at this node's copy. An url the node will not fetch
/// is left as-is, so the page still shows something where it can.
fn rewrite_thumbnails(streams: &mut [PublicStream]) {
    for stream in streams {
        let Some(url) = stream.thumbnail.as_deref() else {
            continue;
        };
        if !super::thumbnails::host_is_allowed(url) {
            continue;
        }
        stream.thumbnail = Some(super::thumbnails::public_path(
            &super::thumbnails::thumbnail_key(url),
        ));
    }
}

/// Refreshes on the configured interval for as long as this node serves the
/// page. Stops when the page moves elsewhere.
pub fn start_streams_refresh(interval: Duration) -> tokio::sync::oneshot::Sender<()> {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        loop {
            refresh_public_streams().await;

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = &mut stop_rx => break,
            }
        }
    });

    stop_tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelPlatforms;
    use crate::plugins::holodex::HolodexChannel;

    fn channel(
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

    fn stream(title: &str, topic: Option<&str>, channel_id: &str) -> HolodexStream {
        HolodexStream {
            id: "vid1".to_string(),
            title: title.to_string(),
            stream_type: "stream".to_string(),
            topic_id: topic.map(str::to_string),
            published_at: None,
            available_at: None,
            status: "live".to_string(),
            start_scheduled: None,
            start_actual: None,
            live_viewers: Some(1200),
            channel: HolodexChannel {
                id: channel_id.to_string(),
                name: "Kamito".to_string(),
                photo: None,
            },
            link: None,
            thumbnail: Some("https://i.ytimg.com/vi/vid1/maxres.jpg".to_string()),
            placeholder_type: None,
        }
    }

    fn kamito() -> Vec<Channel> {
        vec![channel(
            "Kamito",
            &["kmt"],
            Some("UCkamito"),
            Some("kamito_jp"),
        )]
    }

    fn build(streams: Vec<HolodexStream>, enabled: bool, banned: &[&str]) -> Vec<PublicStream> {
        let banned: Vec<String> = banned.iter().map(|k| k.to_string()).collect();
        build_public_streams(streams, &kamito(), enabled, &banned)
    }

    #[test]
    fn a_configured_channel_is_switchable_with_a_command() {
        let built = build(
            vec![stream("ランク", Some("Gaming"), "UCkamito")],
            true,
            &[],
        );

        assert!(built[0].switchable);
        assert_eq!(built[0].reason, None);
        assert_eq!(built[0].command_platform.as_deref(), Some("YT"));
        assert_eq!(built[0].command_channel.as_deref(), Some("Kamito"));
    }

    #[test]
    fn danmaku_disabled_blocks_every_stream() {
        let built = build(vec![stream("ランク", None, "UCkamito")], false, &[]);

        assert!(!built[0].switchable);
        assert_eq!(built[0].reason, Some(NotSwitchable::DanmakuDisabled));
    }

    #[test]
    fn a_banned_keyword_blocks_only_that_stream() {
        let built = build(
            vec![
                stream("Late night ASMR", None, "UCkamito"),
                stream("ランク", None, "UCkamito"),
            ],
            true,
            &["asmr"],
        );

        assert!(!built[0].switchable);
        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
        assert!(built[1].switchable);
    }

    #[test]
    fn the_banned_keyword_itself_is_never_sent() {
        let built = build(
            vec![stream("Late night ASMR", None, "UCkamito")],
            true,
            &["asmr"],
        );
        let json = serde_json::to_string(&built).unwrap();

        assert!(json.contains("banned_keyword"));
        // The title legitimately contains it; the blocklist entry must not
        // appear as its own field.
        assert!(!json.contains("\"keyword\""));
        assert!(!json.contains("\"banned\""));
    }

    #[test]
    fn the_keyword_match_covers_the_topic_too() {
        let built = build(
            vec![stream("配信", Some("Dead by Daylight"), "UCkamito")],
            true,
            &["dead by daylight"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
    }

    #[test]
    fn an_unlisted_channel_cannot_be_requested() {
        let built = build(vec![stream("ランク", None, "UCsomeoneelse")], true, &[]);

        assert!(!built[0].switchable);
        assert_eq!(built[0].reason, Some(NotSwitchable::UnknownChannel));
        assert_eq!(built[0].command_channel, None);
    }

    #[test]
    fn a_twitch_placeholder_resolves_to_the_twitch_command() {
        let mut placeholder = stream("VALORANT", None, "UCkamito");
        placeholder.stream_type = "placeholder".to_string();
        placeholder.link = Some("https://www.twitch.tv/kamito_jp".to_string());

        let built = build(vec![placeholder], true, &[]);

        assert!(built[0].is_placeholder);
        assert_eq!(built[0].command_platform.as_deref(), Some("TW"));
        assert_eq!(built[0].command_channel.as_deref(), Some("Kamito"));
        assert!(built[0].switchable);
    }

    #[test]
    fn a_name_the_parser_could_never_match_is_not_offered() {
        // Every candidate has whitespace, which the command parser strips
        // before comparing, so no typed command could resolve it.
        let channels = vec![channel(
            "Space Name",
            &["also spaced"],
            Some("UCkamito"),
            None,
        )];
        let built = build_public_streams(
            vec![stream("ランク", None, "UCkamito")],
            &channels,
            true,
            &[],
        );

        assert!(!built[0].switchable);
        assert_eq!(built[0].reason, Some(NotSwitchable::NoCommandName));
        assert_eq!(built[0].command_channel, None);
    }

    #[test]
    fn a_spaced_name_falls_back_to_a_usable_alias() {
        let channels = vec![channel(
            "Space Name",
            &["spaced", "kmt"],
            Some("UCkamito"),
            None,
        )];
        let built = build_public_streams(
            vec![stream("ランク", None, "UCkamito")],
            &channels,
            true,
            &[],
        );

        assert_eq!(built[0].command_channel.as_deref(), Some("spaced"));
        assert!(built[0].switchable);
    }

    #[test]
    fn twitch_logins_are_read_out_of_the_link() {
        assert_eq!(
            twitch_login_from_link(Some("https://www.twitch.tv/kamito_jp")).as_deref(),
            Some("kamito_jp")
        );
        assert_eq!(
            twitch_login_from_link(Some("https://twitch.tv/Kamito_JP/videos?x=1")).as_deref(),
            Some("kamito_jp")
        );
        assert_eq!(
            twitch_login_from_link(Some("https://youtube.com/watch")),
            None
        );
        assert_eq!(twitch_login_from_link(None), None);
    }

    #[test]
    fn channel_ids_come_from_channels_json_without_duplicates() {
        let mut cfg = crate::cluster::tests::test_config("ny", 0);
        cfg.youtube.channel_id = "UCkamito".to_string();

        let channels = vec![
            channel("Kamito", &[], Some("UCkamito"), None),
            channel("Nazuna", &[], Some("UCnazuna"), None),
            channel("NoYouTube", &[], None, Some("tw_only")),
        ];

        assert_eq!(
            holodex_channel_ids(&cfg, &channels),
            vec!["UCkamito".to_string(), "UCnazuna".to_string()]
        );
    }

    #[test]
    fn the_configured_channel_is_queried_even_if_unlisted() {
        let mut cfg = crate::cluster::tests::test_config("ny", 0);
        cfg.youtube.channel_id = "UCnotinfile".to_string();

        let ids = holodex_channel_ids(&cfg, &kamito());
        assert_eq!(ids, vec!["UCkamito".to_string(), "UCnotinfile".to_string()]);
    }

    #[test]
    fn serialized_keys_are_the_allowlist() {
        let built = build(
            vec![stream("ランク", Some("Gaming"), "UCkamito")],
            true,
            &[],
        );
        let value = serde_json::to_value(&built[0]).unwrap();
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();

        let mut expected = vec![
            "id",
            "title",
            "topic",
            "status",
            "is_placeholder",
            "start_scheduled",
            "start_actual",
            "live_viewers",
            "channel_name",
            "channel_photo",
            "thumbnail",
            "link",
            "suggested_area_id",
            "suggested_area_name",
            "command_platform",
            "command_channel",
            "switchable",
            "reason",
        ];
        expected.sort();

        assert_eq!(keys, expected);
        // Holodex's channel id is an identifier the page has no use for.
        assert!(!serde_json::to_string(&built[0])
            .unwrap()
            .contains("UCkamito"));
    }
}
