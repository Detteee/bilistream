//! Holodex stream list for the public page.
//!
//! Refreshed on a timer here rather than on request: viewers only ever read a
//! snapshot, so upstream cost is fixed at one call per interval no matter how
//! many people have the page open, and there is no refresh button to hammer.
//!
//! Only channels listed in channels.json are fetched, and the JWT/favorites
//! branch of the Holodex client is never reached.

use axum::body::Bytes;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::{Duration, Instant};

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
    /// `%转播%` names a channel, not a video. An earlier live or upcoming
    /// stream on this channel is banned, so switching would hit that title
    /// first even if this later one is clean.
    EarlierBannedKeyword,
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
fn usable_names(channel: &Channel) -> impl Iterator<Item = &String> {
    std::iter::once(&channel.name)
        .chain(channel.aliases.iter())
        .filter(|candidate| {
            !candidate.trim().is_empty() && !candidate.chars().any(char::is_whitespace)
        })
}

fn command_name(channel: &Channel) -> Option<String> {
    usable_names(channel).next().cloned()
}

fn short_command_name(channel: &Channel) -> Option<String> {
    usable_names(channel)
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

/// `%转播%` names a platform and a channel, not a video, so two Holodex rows
/// for the same YouTube channel (or the same Twitch login) are one switch
/// target. A unique fallback keeps unkeyed rows from grouping together.
fn switch_group_key(stream: &HolodexStream) -> String {
    if let Some(login) = twitch_login_from_link(stream.link.as_deref()) {
        return format!("TW:{login}");
    }
    if !stream.channel.id.is_empty() {
        return format!("YT:{}", stream.channel.id);
    }
    format!("ID:{}", stream.id)
}

/// Live before upcoming, then sooner `start_scheduled`. Same order
/// `select_holodex_channel_status` uses to decide which stream a channel is on.
fn cmp_public_stream_order(a: &PublicStream, b: &PublicStream) -> std::cmp::Ordering {
    let live_rank = |stream: &PublicStream| u8::from(stream.status != "live");
    live_rank(a)
        .cmp(&live_rank(b))
        .then_with(|| match (&a.start_scheduled, &b.start_scheduled) {
            (Some(time_a), Some(time_b)) => time_a.cmp(time_b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        })
}

/// If the stream a channel would actually switch to is banned, later cards on
/// that same target cannot offer 切换 either: the command cannot pick the
/// later video.
fn apply_earlier_banned_keyword(keys: &[String], built: &mut [PublicStream]) {
    // Keep the first row on equal sort keys, just like the previous stable sort.
    let mut earliest: HashMap<&str, usize> = HashMap::with_capacity(keys.len());
    for (index, key) in keys.iter().enumerate() {
        earliest
            .entry(key)
            .and_modify(|current| {
                if cmp_public_stream_order(&built[index], &built[*current]).is_lt() {
                    *current = index;
                }
            })
            .or_insert(index);
    }
    for (index, key) in keys.iter().enumerate() {
        let first = earliest[key.as_str()];
        if first != index
            && built[first].reason == Some(NotSwitchable::BannedKeyword)
            && !built[first].reason_keywords.is_empty()
            && matches!(
                built[index].reason,
                None | Some(NotSwitchable::DanmakuDisabled)
            )
        {
            built[index].reason_keywords = built[first].reason_keywords.clone();
            built[index].reason = Some(NotSwitchable::EarlierBannedKeyword);
            built[index].switchable = false;
        }
    }
}

/// Preserve the first configured channel on duplicate IDs, matching Vec::find.
struct ChannelIndex<'a> {
    youtube: HashMap<&'a str, &'a Channel>,
    twitch: HashMap<String, &'a Channel>,
}

impl<'a> ChannelIndex<'a> {
    fn new(channels: &'a [Channel]) -> Self {
        let mut index = Self {
            youtube: HashMap::with_capacity(channels.len()),
            twitch: HashMap::with_capacity(channels.len()),
        };
        for channel in channels {
            if let Some(id) = channel.platforms.youtube.as_deref() {
                index.youtube.entry(id).or_insert(channel);
            }
            if let Some(id) = channel.platforms.twitch.as_deref() {
                index
                    .twitch
                    .entry(id.to_ascii_lowercase())
                    .or_insert(channel);
            }
        }
        index
    }

    fn command_target(
        &self,
        youtube_channel_id: &str,
        link: Option<&str>,
    ) -> Option<CommandTarget> {
        let (platform, channel) = if let Some(login) = twitch_login_from_link(link) {
            ("TW", *self.twitch.get(&login)?)
        } else {
            ("YT", *self.youtube.get(youtube_channel_id)?)
        };
        Some(CommandTarget {
            platform,
            name: command_name(channel),
            short_name: short_command_name(channel),
        })
    }
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
    let channels = ChannelIndex::new(channels);
    let group_keys: Vec<String> = streams.iter().map(switch_group_key).collect();
    let mut public: Vec<PublicStream> = streams
        .into_iter()
        .map(|stream| {
            let target = channels.command_target(&stream.channel.id, stream.link.as_deref());
            let (reason, reason_keywords) =
                if !platform_is_supported(target.as_ref()) && target.is_some() {
                    (Some(NotSwitchable::UnsupportedPlatform), Vec::new())
                } else {
                    switchability(&stream, target.as_ref(), danmaku_enabled, banned)
                };

            let (suggested_area_id, suggested_area_name) =
                crate::plugins::suggest_area_from_stream(stream.topic_id.as_deref(), &stream.title);
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
        .collect();

    apply_earlier_banned_keyword(&group_keys, &mut public);
    public
}

// Snapshot -------------------------------------------------------------------

struct StreamsSnapshot {
    body: Bytes,
    etag: String,
    confirmed_until: Option<Instant>,
}

impl StreamsSnapshot {
    fn response_at(&self, now: Instant) -> Option<(Bytes, String)> {
        self.confirmed_until.filter(|until| now < *until)?;
        Some((self.body.clone(), self.etag.clone()))
    }
}

static SNAPSHOT: RwLock<Option<StreamsSnapshot>> = RwLock::new(None);

/// A list confirmed within three configured refresh intervals. Repeated
/// upstream failures must eventually return 503, so viewers stop generating
/// commands from indefinitely old eligibility data.
pub(super) fn current_public_streams() -> Option<(Bytes, String)> {
    let guard = SNAPSHOT.read().ok()?;
    let snapshot = guard.as_ref()?;
    snapshot.response_at(Instant::now())
}

fn store_snapshot(streams: &[PublicStream], confirmed_for: Option<Duration>) {
    let Ok(body) = serde_json::to_vec(streams).map(Bytes::from) else {
        return;
    };

    if let Ok(mut guard) = SNAPSHOT.write() {
        replace_streams_snapshot(&mut guard, body, confirmed_for, Instant::now());
    }
}

fn replace_streams_snapshot(
    slot: &mut Option<StreamsSnapshot>,
    body: Bytes,
    confirmed_for: Option<Duration>,
    now: Instant,
) {
    // A keyword/area remap changes the body, not the age of its source data.
    let confirmed_until = confirmed_for
        .and_then(|limit| now.checked_add(limit))
        .or_else(|| slot.as_ref().and_then(|snapshot| snapshot.confirmed_until));
    if let Some(previous) = slot.as_mut() {
        if previous.body == body {
            previous.confirmed_until = confirmed_until;
            return;
        }
    }
    *slot = Some(StreamsSnapshot {
        etag: super::body_etag(&body),
        body,
        confirmed_until,
    });
}

/// YouTube channel ids to ask Holodex about: exactly what channels.json lists,
/// plus whatever channel is configured right now.
fn holodex_channel_ids(cfg: &Config, channels: &[Channel]) -> Vec<String> {
    let mut seen = HashSet::with_capacity(channels.len() + 1);
    channels
        .iter()
        .filter_map(|channel| channel.platforms.youtube.as_deref())
        .chain(std::iter::once(cfg.youtube.channel_id.trim()))
        .filter(|id| !id.is_empty() && seen.insert(*id))
        .map(str::to_owned)
        .collect()
}

/// One refresh cycle. Returns false when the list could not be refreshed, so
/// the caller knows the snapshot it still holds is the last good one.
async fn refresh_public_streams() -> bool {
    rebuild_public_streams(false).await
}

/// Re-applies current areas.json keywords and danmaku bans to the last Holodex
/// payload so a management edit shows up without waiting for the poll timer.
pub fn remap_after_areas_change() {
    tokio::spawn(async {
        let _ = rebuild_public_streams(true).await;
    });
}

async fn rebuild_public_streams(cached_only: bool) -> bool {
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

    let streams = if cached_only {
        match super::holodex_cache::get_cached(&ids) {
            Some(streams) => streams,
            None => return false,
        }
    } else {
        // A dashboard fetch on this node counts as this interval's call, so the
        // two together stay at one upstream request per interval.
        let max_age = Duration::from_secs(cfg.cluster.public_status.holodex_refresh_secs);
        match super::holodex_cache::get_or_fetch(ids.clone(), max_age).await {
            Ok(streams) => streams,
            Err(e) => {
                tracing::debug!("公开状态页 Holodex 刷新失败: {}", e);
                return false;
            }
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

    // A failed poll returns early above and leaves the cache alone. Cached-only
    // remaps reuse the last Holodex payload after areas.json changed.
    // Channel photos use the same cache: the page's CSP only allows same-origin
    // images, and those CDNs are often unreachable from the viewer's network.
    super::thumbnails::reconcile(&image_urls(&public)).await;
    rewrite_thumbnails(&mut public);

    let confirmed_for = (!cached_only).then(|| {
        Duration::from_secs(
            cfg.cluster
                .public_status
                .holodex_refresh_secs
                .max(30)
                .saturating_mul(3),
        )
    });
    store_snapshot(&public, confirmed_for);
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

    #[test]
    fn unchanged_success_renews_freshness_but_remapping_old_streams_does_not() {
        let now = Instant::now();
        let valid_for = Duration::from_secs(90);
        let mut slot = None;
        let body = Bytes::from_static(b"[]");
        replace_streams_snapshot(&mut slot, body.clone(), Some(valid_for), now);
        let etag = slot.as_ref().unwrap().etag.clone();
        assert!(slot
            .as_ref()
            .unwrap()
            .response_at(now + valid_for)
            .is_none());
        replace_streams_snapshot(
            &mut slot,
            body,
            Some(valid_for),
            now + Duration::from_secs(60),
        );
        assert_eq!(slot.as_ref().unwrap().etag, etag);
        assert!(slot
            .as_ref()
            .unwrap()
            .response_at(now + valid_for)
            .is_some());
        replace_streams_snapshot(
            &mut slot,
            Bytes::from_static(b"[1]"),
            None,
            now + Duration::from_secs(140),
        );
        assert!(slot
            .as_ref()
            .unwrap()
            .response_at(now + Duration::from_secs(150))
            .is_none());
        let mut unconfirmed = None;
        replace_streams_snapshot(&mut unconfirmed, Bytes::from_static(b"[]"), None, now);
        assert!(unconfirmed.as_ref().unwrap().response_at(now).is_none());
    }

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
        stream_on("vid1", title, topic, channel_id, "live", None)
    }

    fn stream_on(
        id: &str,
        title: &str,
        topic: Option<&str>,
        channel_id: &str,
        status: &str,
        scheduled: Option<&str>,
    ) -> HolodexStream {
        HolodexStream {
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
            channel: HolodexChannel {
                id: channel_id.to_string(),
                name: "Kamito".to_string(),
                photo: None,
            },
            link: None,
            thumbnail: Some(format!("https://i.ytimg.com/vi/{id}/maxres.jpg")),
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

    /// A keyword on one channel must not grey out a different channel's card.
    #[test]
    fn a_banned_keyword_blocks_only_that_channel() {
        let channels = vec![
            channel("Kamito", &["kmt"], Some("UCkamito"), Some("kamito_jp")),
            channel("Nazuna", &["nzn"], Some("UCnazuna"), None),
        ];
        let banned = vec!["asmr".to_string()];
        let built = build_public_streams(
            vec![
                stream("Late night ASMR", None, "UCkamito"),
                stream("ランク", None, "UCnazuna"),
            ],
            &channels,
            true,
            &banned,
        );

        assert!(!built[0].switchable);
        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
        assert!(built[1].switchable);
        assert_eq!(built[1].reason, None);
    }

    /// `%转播%` names the channel. If the sooner stream is banned, the later
    /// clean title is not requestable either — switching would hit 雑談 first.
    #[test]
    fn an_earlier_banned_stream_blocks_later_ones_on_the_same_channel() {
        let built = build(
            vec![
                stream_on(
                    "morning",
                    "【朝活雑談】 もう9月!!!!!!!!!!",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T06:00:00+09:00"),
                ),
                stream_on(
                    "evening",
                    "【 Phasmophobia 】 ウンウンウンウンOK幽霊ね!!!!!",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T20:00:00+09:00"),
                ),
            ],
            true,
            &["雑談"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
        assert_eq!(built[0].reason_keywords, vec!["雑談".to_string()]);
        assert!(!built[1].switchable);
        assert_eq!(built[1].reason, Some(NotSwitchable::EarlierBannedKeyword));
        assert_eq!(built[1].reason_keywords, vec!["雑談".to_string()]);
    }

    /// Same as a per-stream keyword block: keep showing 标题/分区包含 on the
    /// later card while 转播 has the processor down, so a stale danmaku_disabled
    /// reason cannot turn 切换 green when the gate reopens.
    #[test]
    fn an_earlier_ban_outweighs_the_global_gate_on_later_cards() {
        let built = build(
            vec![
                stream_on(
                    "morning",
                    "【朝活雑談】",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T06:00:00+09:00"),
                ),
                stream_on(
                    "evening",
                    "【 Phasmophobia 】",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T20:00:00+09:00"),
                ),
            ],
            false,
            &["雑談"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
        assert_eq!(built[1].reason, Some(NotSwitchable::EarlierBannedKeyword));
        assert_eq!(built[1].reason_keywords, vec!["雑談".to_string()]);
    }

    /// List order is not schedule order: the 06:00 雑談 still blocks the
    /// 20:00 card when Holodex returns the later row first.
    #[test]
    fn the_sooner_banned_stream_wins_even_when_listed_second() {
        let built = build(
            vec![
                stream_on(
                    "evening",
                    "【 Phasmophobia 】",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T20:00:00+09:00"),
                ),
                stream_on(
                    "morning",
                    "【朝活雑談】",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T06:00:00+09:00"),
                ),
            ],
            true,
            &["雑談"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::EarlierBannedKeyword));
        assert_eq!(built[1].reason, Some(NotSwitchable::BannedKeyword));
    }

    /// The later title is what `%转播%` would hit only after the sooner one
    /// is gone, so a clean earlier stream stays requestable.
    #[test]
    fn a_later_banned_stream_does_not_block_an_earlier_clean_one() {
        let built = build(
            vec![
                stream_on(
                    "morning",
                    "ランク",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T06:00:00+09:00"),
                ),
                stream_on(
                    "evening",
                    "Late night ASMR",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T20:00:00+09:00"),
                ),
            ],
            true,
            &["asmr"],
        );

        assert!(built[0].switchable);
        assert_eq!(built[0].reason, None);
        assert_eq!(built[1].reason, Some(NotSwitchable::BannedKeyword));
    }

    /// Twitch is a different `%转播%` target than YouTube, even when Holodex
    /// keys the placeholder by the same YouTube channel.
    #[test]
    fn an_earlier_youtube_ban_does_not_block_a_later_twitch_placeholder() {
        let mut twitch = stream_on(
            "twitch-eve",
            "VALORANT",
            None,
            "UCkamito",
            "upcoming",
            Some("2026-09-02T20:00:00+09:00"),
        );
        twitch.stream_type = "placeholder".to_string();
        twitch.link = Some("https://www.twitch.tv/kamito_jp".to_string());

        let built = build(
            vec![
                stream_on(
                    "morning",
                    "【朝活雑談】",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T06:00:00+09:00"),
                ),
                twitch,
            ],
            true,
            &["雑談"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::BannedKeyword));
        assert!(built[1].switchable);
        assert_eq!(built[1].command_platform.as_deref(), Some("TW"));
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
