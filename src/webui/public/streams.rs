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
use crate::plugins::banned_keywords::{banned_keyword_hits, danmaku_banned_keywords};
use crate::plugins::holodex::HolodexStream;

/// Why a stream cannot be requested. Banned-keyword hits go in
/// `reason_keywords` rather than this enum, so the page can name the words
/// without a new variant per match.
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
    pub placeholder_type: Option<String>,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub available_at: Option<String>,
    pub published_at: Option<String>,
    pub live_viewers: Option<i32>,
    pub channel_name: String,
    pub channel_photo: Option<String>,
    pub thumbnail: Option<String>,
    pub link: Option<String>,
    pub suggested_area_id: Option<u64>,
    pub suggested_area_name: Option<String>,
    /// `YT` or `TW`, matching what the danmaku command expects.
    pub command_platform: Option<String>,
    /// The formal channel name to put in the command, or the first usable
    /// alias when the name itself is unusable.
    pub command_channel: Option<String>,
    /// The shortest usable name or alias, for when the formal command does not
    /// fit a regular user's danmaku.
    pub command_channel_short: Option<String>,
    pub switchable: bool,
    pub reason: Option<NotSwitchable>,
    /// The keywords that blocked it, so the page can say which words matched
    /// rather than leaving the block looking arbitrary. They are already
    /// visible in the title or topic being shown.
    pub reason_keywords: Vec<String>,
}

/// What the page needs to know about a configured channel to build a command.
struct CommandTarget {
    platform: &'static str,
    name: Option<String>,
    short_name: Option<String>,
}

/// The command parser strips whitespace before matching against channels.json,
/// so a name containing any is unusable no matter what the viewer types.
fn usable_names(channel: &Channel) -> Vec<&String> {
    std::iter::once(&channel.name)
        .chain(channel.aliases.iter())
        .filter(|candidate| {
            !candidate.trim().is_empty() && !candidate.chars().any(char::is_whitespace)
        })
        .collect()
}

/// The formal name when it is usable, otherwise the first usable alias.
fn command_name(channel: &Channel) -> Option<String> {
    usable_names(channel).first().map(|name| (*name).clone())
}

/// The shortest usable name or alias, for the short form of the command.
fn short_command_name(channel: &Channel) -> Option<String> {
    usable_names(channel)
        .into_iter()
        .min_by_key(|name| name.chars().count())
        .cloned()
}

/// Holodex omits the thumbnail for plain YouTube streams and for Twitch
/// placeholders, so derive what the dashboard derives. Done here rather than
/// in the page so the cache fetches these too.
fn thumbnail_for(stream: &HolodexStream) -> Option<String> {
    if let Some(thumbnail) = stream.thumbnail.as_deref().filter(|t| !t.is_empty()) {
        return Some(thumbnail.to_string());
    }

    if stream.stream_type == "placeholder" {
        return twitch_login_from_link(stream.link.as_deref()).map(|login| {
            format!("https://static-cdn.jtvnw.net/previews-ttv/live_user_{login}-640x360.jpg")
        });
    }

    (!stream.id.is_empty()).then(|| format!("https://i.ytimg.com/vi/{}/sddefault.jpg", stream.id))
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
            short_name: short_command_name(channel),
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
        short_name: short_command_name(matched),
    })
}

/// Whether a stream can be requested, and why not when it cannot.
///
/// Per-stream reasons (unknown channel, banned title) are computed even when
/// the owner's processor gate is down. The page greys every 切换 button from
/// the live status payload; baking the gate in as the only reason would hide
/// a keyword block and, after 转播 ends, leave every card stuck unswitchable
/// until the 30s Holodex snapshot rebuilt.
fn switchability(
    stream: &HolodexStream,
    target: Option<&CommandTarget>,
    danmaku_enabled: bool,
    banned: &[String],
) -> (Option<NotSwitchable>, Vec<String>) {
    let Some(target) = target else {
        return (Some(NotSwitchable::UnknownChannel), Vec::new());
    };
    if target.name.is_none() {
        return (Some(NotSwitchable::NoCommandName), Vec::new());
    }

    let haystack = crate::plugins::banned_keywords::danmaku_haystack(
        stream.topic_id.as_deref().unwrap_or_default(),
        &stream.title,
    );
    let hits = banned_keyword_hits(&haystack, banned);
    if !hits.is_empty() {
        // Title matches first, then topic, matching the "标题/分区包含" wording.
        let title = stream.title.to_lowercase();
        let (in_title, in_topic): (Vec<_>, Vec<_>) = hits
            .into_iter()
            .partition(|keyword| title.contains(keyword.as_str()));
        let mut ordered = in_title;
        ordered.extend(in_topic);
        return (Some(NotSwitchable::BannedKeyword), ordered);
    }

    if !danmaku_enabled {
        return (Some(NotSwitchable::DanmakuDisabled), Vec::new());
    }

    (None, Vec::new())
}

/// Niconico and anything else Holodex surfaces cannot be requested, because
/// the danmaku command only parses YT and TW.
fn platform_is_supported(target: Option<&CommandTarget>) -> bool {
    matches!(target, Some(t) if t.platform == "YT" || t.platform == "TW")
}

pub(super) fn build_public_streams(
    streams: Vec<HolodexStream>,
    channels: &[Channel],
    danmaku_enabled: bool,
    banned: &[String],
) -> Vec<PublicStream> {
    streams
        .into_iter()
        .map(|stream| {
            let target = command_target(channels, &stream.channel.id, stream.link.as_deref());
            let (reason, reason_keywords) =
                if !platform_is_supported(target.as_ref()) && target.is_some() {
                    (Some(NotSwitchable::UnsupportedPlatform), Vec::new())
                } else {
                    switchability(&stream, target.as_ref(), danmaku_enabled, banned)
                };

            let (suggested_area_id, suggested_area_name) = suggested_area(&stream);
            let thumbnail = thumbnail_for(&stream);

            PublicStream {
                id: stream.id,
                title: stream.title,
                topic: stream.topic_id,
                status: stream.status,
                is_placeholder: stream.stream_type == "placeholder",
                placeholder_type: stream.placeholder_type,
                start_scheduled: stream.start_scheduled,
                start_actual: stream.start_actual,
                available_at: stream.available_at,
                published_at: stream.published_at,
                live_viewers: stream.live_viewers,
                channel_name: stream.channel.name,
                channel_photo: stream.channel.photo.filter(|photo| !photo.is_empty()),
                thumbnail,
                link: stream.link,
                suggested_area_id,
                suggested_area_name,
                command_platform: target.as_ref().map(|target| target.platform.to_string()),
                command_channel_short: target.as_ref().and_then(|target| target.short_name.clone()),
                command_channel: target.and_then(|target| target.name),
                switchable: reason.is_none(),
                reason,
                reason_keywords,
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
pub(super) fn current_public_streams() -> Option<(String, String)> {
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
async fn refresh_public_streams() -> bool {
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
    let streams = match super::holodex_cache::get_or_fetch(ids.clone(), max_age).await {
        Ok(streams) => streams,
        Err(e) => {
            tracing::debug!("公开状态页 Holodex 刷新失败: {}", e);
            return false;
        }
    };

    // Same filter the dashboard applies: a collab shows up under a channel we
    // do not monitor, and an upcoming entry is noise once the channel is live
    // or if it is more than a day out.
    let streams = crate::webui::api::filter_holodex_streams(streams, ids.into_iter().collect());

    // Config stays on during a restream; the processor gate does not. Use the
    // same owner-facing flag the status cards do, or 切换 stays green while
    // `%转播%` is rejected.
    let cluster = crate::cluster::get_cluster_status_for_config(&cfg).await;
    let danmaku_enabled = super::snapshot::public_danmaku_enabled(&cluster);

    let banned = danmaku_banned_keywords();
    let mut public = build_public_streams(streams, &channels, danmaku_enabled, &banned);

    // Only a successful fetch reaches here, so the list is safe to reconcile
    // against; a failed poll returns early above and leaves the cache alone.
    // Channel photos use the same cache: the page's CSP only allows same-origin
    // images, and those CDNs are often unreachable from the viewer's network.
    super::thumbnails::reconcile(&image_urls(&public)).await;
    rewrite_thumbnails(&mut public);

    store_snapshot(&public);
    true
}

/// Thumbnails and avatars the node will fetch for this list.
fn image_urls(streams: &[PublicStream]) -> Vec<String> {
    streams
        .iter()
        .flat_map(|stream| [stream.thumbnail.clone(), stream.channel_photo.clone()])
        .flatten()
        .collect()
}

/// Points each image at this node's copy. An url the node will not fetch is
/// dropped: the page's CSP only allows same-origin images, so leaving the
/// upstream address would never paint.
fn rewrite_thumbnails(streams: &mut [PublicStream]) {
    for stream in streams {
        rewrite_image(&mut stream.thumbnail);
        rewrite_image(&mut stream.channel_photo);
    }
}

fn rewrite_image(url: &mut Option<String>) {
    let Some(src) = url.as_deref() else {
        return;
    };
    if super::thumbnails::host_is_allowed(src) {
        *url = Some(super::thumbnails::public_path(
            &super::thumbnails::thumbnail_key(src),
        ));
    } else {
        *url = None;
    }
}

/// Refreshes on the configured interval for as long as this node serves the
/// page. Stops when the page moves elsewhere.
pub(super) fn start_streams_refresh(interval: Duration) -> tokio::sync::oneshot::Sender<()> {
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

    /// A keyword block is still a keyword block while 转播 has the processor
    /// down, so the page can keep showing 标题/分区包含 after the gate reopens
    /// without waiting for a Holodex rebuild.
    #[test]
    fn a_banned_keyword_outweighs_the_global_gate() {
        let built = build(
            vec![stream("Late night ASMR", None, "UCkamito")],
            false,
            &["asmr"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
        assert_eq!(built[0].reason_keywords, vec!["asmr".to_string()]);
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

    /// Without the words, a block reads as arbitrary. They are already on
    /// screen in the title or topic the card shows.
    #[test]
    fn a_block_reports_which_keywords_matched() {
        let built = build(
            vec![stream("【雑談】カルピス", Some("talk"), "UCkamito")],
            true,
            &["talk", "asmr", "雑談"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
        assert_eq!(
            built[0].reason_keywords,
            vec!["雑談".to_string(), "talk".to_string()]
        );
    }

    #[test]
    fn other_block_reasons_carry_no_keywords() {
        let built = build(vec![stream("ランク", None, "UCkamito")], false, &["asmr"]);

        assert_eq!(built[0].reason, Some(NotSwitchable::DanmakuDisabled));
        assert!(built[0].reason_keywords.is_empty());
    }

    #[test]
    fn a_switchable_stream_carries_no_keywords() {
        let built = build(vec![stream("ランク", None, "UCkamito")], true, &["asmr"]);

        assert!(built[0].switchable);
        assert!(built[0].reason_keywords.is_empty());
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

    /// Holodex leaves these null, and a card with no image is what the page
    /// showed before the fallback existed.
    #[test]
    fn a_youtube_stream_without_a_thumbnail_falls_back_to_the_video_still() {
        let mut yt = stream("ランク", None, "UCkamito");
        yt.thumbnail = None;

        assert_eq!(
            thumbnail_for(&yt).as_deref(),
            Some("https://i.ytimg.com/vi/vid1/sddefault.jpg")
        );
    }

    #[test]
    fn a_twitch_placeholder_falls_back_to_the_channel_preview() {
        let mut placeholder = stream("VALORANT", None, "UCkamito");
        placeholder.stream_type = "placeholder".to_string();
        placeholder.link = Some("https://www.twitch.tv/kamito_jp".to_string());
        placeholder.thumbnail = None;

        assert_eq!(
            thumbnail_for(&placeholder).as_deref(),
            Some("https://static-cdn.jtvnw.net/previews-ttv/live_user_kamito_jp-640x360.jpg")
        );
    }

    #[test]
    fn a_thumbnail_holodex_does_supply_is_kept() {
        let yt = stream("ランク", None, "UCkamito");
        assert_eq!(
            thumbnail_for(&yt).as_deref(),
            Some("https://i.ytimg.com/vi/vid1/maxres.jpg")
        );
    }

    #[test]
    fn an_empty_thumbnail_is_treated_as_missing() {
        let mut yt = stream("ランク", None, "UCkamito");
        yt.thumbnail = Some(String::new());
        assert_eq!(
            thumbnail_for(&yt).as_deref(),
            Some("https://i.ytimg.com/vi/vid1/sddefault.jpg")
        );
    }

    #[test]
    fn channel_photos_are_fetched_alongside_thumbnails() {
        let mut yt = stream("ランク", None, "UCkamito");
        yt.channel.photo = Some("https://yt3.ggpht.com/a.jpg".to_string());
        let built = build(vec![yt], true, &[]);
        let urls = image_urls(&built);

        assert!(urls.iter().any(|url| url == "https://yt3.ggpht.com/a.jpg"));
        assert!(urls
            .iter()
            .any(|url| url == "https://i.ytimg.com/vi/vid1/maxres.jpg"));
    }

    #[test]
    fn an_image_from_a_host_we_will_not_fetch_is_dropped() {
        let mut url = Some("https://evil.example/x.jpg".to_string());
        rewrite_image(&mut url);
        assert_eq!(url, None);
    }

    #[test]
    fn an_allowed_image_is_rewritten_to_the_local_path() {
        let src = "https://i.ytimg.com/vi/vid1/maxres.jpg";
        let mut url = Some(src.to_string());
        rewrite_image(&mut url);
        let rewritten = url.expect("rewritten");
        assert!(rewritten.starts_with("/t/"));
        assert_ne!(rewritten, src);
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
            "placeholder_type",
            "start_scheduled",
            "start_actual",
            "available_at",
            "published_at",
            "live_viewers",
            "channel_name",
            "channel_photo",
            "thumbnail",
            "link",
            "suggested_area_id",
            "suggested_area_name",
            "command_platform",
            "command_channel",
            "command_channel_short",
            "switchable",
            "reason",
            "reason_keywords",
        ];
        expected.sort();

        assert_eq!(keys, expected);
        // Holodex's channel id is an identifier the page has no use for.
        assert!(!serde_json::to_string(&built[0])
            .unwrap()
            .contains("UCkamito"));
    }
}
