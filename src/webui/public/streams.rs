//! Stream list for the public page.
//!
//! Built from `main`'s channels list (`webui::holodex_list`), the rows the
//! dashboard shows, whenever that list or the cluster changes. Viewers only
//! ever read the snapshot, so their traffic never reaches Holodex or Google.

use axum::body::Bytes;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Channel;
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
/// for the same YouTube channel (or the same Twitch login, or the same
/// Niconico program) are one switch target. A unique fallback keeps unkeyed
/// rows from grouping together.
fn switch_group_key(stream: &HolodexStream) -> String {
    if let Some(live_id) = stream
        .link
        .as_deref()
        .and_then(crate::plugins::live_id_from_link)
    {
        return format!("NC:{live_id}");
    }
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
        let (platform, channel) = if crate::plugins::live_id_from_link(link.unwrap_or("")).is_some()
        {
            ("NC", *self.youtube.get(youtube_channel_id)?)
        } else if let Some(login) = twitch_login_from_link(link) {
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

/// The list rebuilds from `main`'s channels list whenever it, the cluster (the
/// danmaku gate) or the areas change, and at least this often, which keeps the
/// channels list leased (10 min) while the page is served.
const KEEP_LEASE: Duration = Duration::from_secs(4 * 60);
/// Events arriving together share one rebuild.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// Holodex counts as fresh for this many of its intervals, so two missed
/// backstop fetches in a row are tolerated.
const HOLODEX_GRACE_INTERVALS: u32 = 3;

struct StreamsSnapshot {
    body: Bytes,
    etag: String,
    /// When Holodex last answered for the list this body came from.
    holodex_ok_at: Option<Instant>,
    holodex_every: Duration,
}

impl StreamsSnapshot {
    /// Served while either source is healthy: Holodex answered within
    /// `HOLODEX_GRACE_INTERVALS` of its cadence, or the YouTube store answers.
    /// With neither, viewers get 503 rather than commands built on stale data.
    fn response_at(&self, now: Instant, youtube_answering: bool) -> Option<(Bytes, String)> {
        let holodex_fresh = self.holodex_ok_at.is_some_and(|at| {
            now.saturating_duration_since(at) < self.holodex_every * HOLODEX_GRACE_INTERVALS
        });
        (holodex_fresh || youtube_answering).then(|| (self.body.clone(), self.etag.clone()))
    }
}

static SNAPSHOT: RwLock<Option<StreamsSnapshot>> = RwLock::new(None);

pub(super) async fn current_public_streams() -> Option<(Bytes, String)> {
    let youtube_answering = crate::config::load_config()
        .await
        .is_ok_and(|cfg| crate::plugins::youtube_index::answering(&cfg));
    let guard = SNAPSHOT.read().ok()?;
    guard
        .as_ref()?
        .response_at(Instant::now(), youtube_answering)
}

fn store_snapshot(
    streams: &[PublicStream],
    holodex_ok_at: Option<Instant>,
    holodex_every: Duration,
) {
    let Ok(body) = serde_json::to_vec(streams).map(Bytes::from) else {
        return;
    };
    if let Ok(mut guard) = SNAPSHOT.write() {
        replace_streams_snapshot(&mut guard, body, holodex_ok_at, holodex_every);
    }
}

/// An unchanged body keeps its ETag, so viewers' polls settle for a 304.
fn replace_streams_snapshot(
    slot: &mut Option<StreamsSnapshot>,
    body: Bytes,
    holodex_ok_at: Option<Instant>,
    holodex_every: Duration,
) {
    if let Some(previous) = slot.as_mut() {
        if previous.body == body {
            previous.holodex_ok_at = holodex_ok_at;
            previous.holodex_every = holodex_every;
            return;
        }
    }
    *slot = Some(StreamsSnapshot {
        etag: super::body_etag(&body),
        body,
        holodex_ok_at,
        holodex_every,
    });
}

/// Re-applies current areas.json keywords and danmaku bans after an edit,
/// without waiting for the next list change.
pub fn remap_after_areas_change() {
    tokio::spawn(async {
        rebuild_public_streams().await;
    });
}

/// One rebuild from the channels list. Returns false when there is no list,
/// leaving the last snapshot to age out.
async fn rebuild_public_streams() -> bool {
    let Ok(cfg) = crate::config::load_config().await else {
        return false;
    };
    let Ok(channels_data) = crate::config::load_channels() else {
        return false;
    };
    let channels = channels_data.channels;

    // Already merged with discovery, overlaid by YouTube and filtered, the same
    // rows the dashboard shows. Reading it also extends the list's lease.
    let list = match crate::webui::holodex_list::current(
        crate::webui::holodex_list::ListKind::Channels,
        false,
    )
    .await
    {
        Ok(list) => list,
        Err(e) => {
            tracing::debug!("公开状态页读取直播列表失败: {}", e);
            return false;
        }
    };

    // Config stays on during a restream; the processor gate does not. Use the
    // same owner-facing flag the status cards do, or 切换 stays green while
    // `%转播%` is rejected.
    let cluster = crate::cluster::get_cluster_status_for_config(&cfg).await;
    let danmaku_enabled = super::snapshot::public_danmaku_enabled(&cluster);

    let banned = danmaku_banned_keywords();
    let mut public = build_public_streams(list.rows.clone(), &channels, danmaku_enabled, &banned);

    // Channel photos use the same cache: the page's CSP only allows same-origin
    // images, and those CDNs are often unreachable from the viewer's network.
    super::thumbnails::reconcile(&image_urls(&public)).await;
    rewrite_thumbnails(&mut public);

    store_snapshot(&public, list.holodex_ok_at, list.holodex_every);
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

/// Whether a server event can change the public list: the channels list
/// changed, the cluster (the danmaku gate) changed, or events were missed.
fn rebuild_on(
    event: Result<&'static str, tokio::sync::broadcast::error::RecvError>,
) -> Option<bool> {
    use tokio::sync::broadcast::error::RecvError;
    match event {
        Ok(kind) => {
            Some(kind == crate::webui::events::HOLODEX || kind == crate::webui::events::CLUSTER)
        }
        Err(RecvError::Lagged(_)) => Some(true),
        Err(RecvError::Closed) => None,
    }
}

/// Rebuilds on list and cluster events, and every `KEEP_LEASE`, for as long as
/// this node serves the page. Stops when the page moves elsewhere.
pub(super) fn start_streams_watch() -> tokio::sync::oneshot::Sender<()> {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut events = crate::AppState::current().subscribe_events();
        loop {
            rebuild_public_streams().await;
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    _ = tokio::time::sleep(KEEP_LEASE) => break,
                    event = events.recv() => match rebuild_on(event) {
                        Some(true) => {
                            tokio::time::sleep(DEBOUNCE).await;
                            // Events in the window share this rebuild.
                            while events.try_recv().is_ok() {}
                            break;
                        }
                        Some(false) => {}
                        None => return,
                    },
                }
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
    use crate::plugins::youtube_data::{YtLiveDetails, YtSnippet, YtVideo};
    use std::collections::BTreeMap;

    #[test]
    fn the_list_is_served_while_either_source_is_healthy() {
        let now = Instant::now();
        let every = Duration::from_secs(300);
        let mut slot = None;
        let body = Bytes::from_static(b"[]");
        replace_streams_snapshot(&mut slot, body.clone(), Some(now), every);
        let snapshot = slot.as_ref().unwrap();
        let stale = now + every * HOLODEX_GRACE_INTERVALS;
        assert!(
            snapshot
                .response_at(stale - Duration::from_secs(1), false)
                .is_some(),
            "Holodex only"
        );
        assert!(snapshot.response_at(stale, true).is_some(), "YouTube only");
        assert!(snapshot.response_at(stale, false).is_none(), "neither: 503");

        let mut never = None;
        replace_streams_snapshot(&mut never, body.clone(), None, every);
        assert!(never.as_ref().unwrap().response_at(now, false).is_none());
        assert!(never.as_ref().unwrap().response_at(now, true).is_some());
    }

    #[test]
    fn an_unchanged_body_keeps_its_etag_and_takes_the_new_holodex_time() {
        let now = Instant::now();
        let every = Duration::from_secs(60);
        let mut slot = None;
        replace_streams_snapshot(&mut slot, Bytes::from_static(b"[]"), Some(now), every);
        let etag = slot.as_ref().unwrap().etag.clone();
        let later = now + Duration::from_secs(120);
        replace_streams_snapshot(&mut slot, Bytes::from_static(b"[]"), Some(later), every);
        assert_eq!(slot.as_ref().unwrap().etag, etag);
        assert_eq!(slot.as_ref().unwrap().holodex_ok_at, Some(later));
        replace_streams_snapshot(&mut slot, Bytes::from_static(b"[1]"), Some(later), every);
        assert_ne!(slot.as_ref().unwrap().etag, etag);
    }

    #[test]
    fn only_list_and_cluster_events_rebuild() {
        use tokio::sync::broadcast::error::RecvError;
        assert_eq!(rebuild_on(Ok(crate::webui::events::HOLODEX)), Some(true));
        assert_eq!(rebuild_on(Ok(crate::webui::events::CLUSTER)), Some(true));
        assert_eq!(rebuild_on(Ok(crate::webui::events::STATUS)), Some(false));
        assert_eq!(rebuild_on(Err(RecvError::Lagged(3))), Some(true));
        assert_eq!(rebuild_on(Err(RecvError::Closed)), None);
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
            yt_confirmed: false,
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

    /// Holodex does not list Niconico, so overlay rows are placeholders keyed
    /// by the YouTube channel. `%转播%` still cannot name NC.
    #[test]
    fn a_niconico_placeholder_cannot_be_requested() {
        let mut placeholder = stream("激ロー", None, "UCkamito");
        placeholder.stream_type = "placeholder".to_string();
        placeholder.link = Some("https://live.nicovideo.jp/watch/lv351182284".to_string());

        let built = build(vec![placeholder], true, &[]);

        assert!(built[0].is_placeholder);
        assert!(!built[0].switchable);
        assert_eq!(built[0].reason, Some(NotSwitchable::UnsupportedPlatform));
        assert_eq!(built[0].command_platform.as_deref(), Some("NC"));
        assert_eq!(built[0].command_channel.as_deref(), Some("Kamito"));
    }

    /// Niconico is a different target than YouTube, even when the overlay
    /// keys the placeholder by the same YouTube channel.
    #[test]
    fn a_niconico_placeholder_does_not_block_youtube_on_the_same_channel() {
        let mut niconico = stream_on(
            "niconico-lv351182284",
            "雑談",
            None,
            "UCkamito",
            "live",
            None,
        );
        niconico.stream_type = "placeholder".to_string();
        niconico.link = Some("https://live.nicovideo.jp/watch/lv351182284".to_string());

        let built = build(
            vec![
                niconico,
                stream_on(
                    "morning",
                    "ランク",
                    None,
                    "UCkamito",
                    "upcoming",
                    Some("2026-09-02T20:00:00+09:00"),
                ),
            ],
            true,
            &["雑談"],
        );

        assert_eq!(built[0].reason, Some(NotSwitchable::UnsupportedPlatform));
        assert!(built[1].switchable);
        assert_eq!(built[1].command_platform.as_deref(), Some("YT"));
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
    fn a_niconico_placeholder_has_no_youtube_thumbnail() {
        let mut placeholder = stream("激ロー", None, "UCkamito");
        placeholder.id = "niconico-lv351182284".to_string();
        placeholder.stream_type = "placeholder".to_string();
        placeholder.link = Some("https://live.nicovideo.jp/watch/lv351182284".to_string());
        placeholder.thumbnail = None;

        assert_eq!(thumbnail_for(&placeholder), None);
    }

    #[test]
    fn a_niconico_listing_thumbnail_is_kept() {
        let mut placeholder = stream("激ロー", None, "UCkamito");
        placeholder.id = "niconico-lv351182284".to_string();
        placeholder.stream_type = "placeholder".to_string();
        placeholder.link = Some("https://live.nicovideo.jp/watch/lv351182284".to_string());
        placeholder.thumbnail = Some(
            "https://listing-thumbnail.live.nicovideo.jp?image=prod-lv351182284/t.jpg&w=640&h=360"
                .to_string(),
        );

        assert_eq!(
            thumbnail_for(&placeholder).as_deref(),
            Some(
                "https://listing-thumbnail.live.nicovideo.jp?image=prod-lv351182284/t.jpg&w=640&h=360"
            )
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

    fn youtube_answer(id: &str, end: Option<&str>) -> YtVideo {
        YtVideo {
            id: id.to_string(),
            snippet: YtSnippet::default(),
            live_streaming_details: Some(YtLiveDetails {
                actual_start_time: Some("2026-09-25T10:00:00Z".to_string()),
                actual_end_time: end.map(str::to_string),
                ..Default::default()
            }),
        }
    }

    /// The dashboard's corrections reach the public list: a waiting room
    /// YouTube says ended is gone even though Holodex still lists it, and a
    /// stream Holodex dropped during an encoder outage stays while YouTube
    /// says it is live. They are on different channels, since a live stream
    /// would hide its own channel's waiting room anyway.
    #[tokio::test]
    async fn the_public_list_follows_the_youtube_index() {
        let mut dropped = stream_on("dropped", "ランク", None, "UCnazuna", "live", None);
        dropped.yt_confirmed = true;
        let index = crate::cluster::YtIndexPayload {
            version: "v1".to_string(),
            videos: BTreeMap::from([
                (
                    "ended".to_string(),
                    Some(youtube_answer("ended", Some("2026-09-25T11:00:00Z"))),
                ),
                ("dropped".to_string(), Some(youtube_answer("dropped", None))),
            ]),
            discovered: vec![dropped],
        };
        let holodex = vec![stream_on(
            "ended", "雑談", None, "UCkamito", "upcoming", None,
        )];

        let rows = crate::cluster::with_yt_index_role(
            crate::cluster::YtIndexRole::Peer(std::sync::Arc::new(index)),
            // The channels list's steps (`holodex_list::build`).
            async {
                let rows = crate::plugins::youtube_rss::merge_discovered(holodex);
                let rows = crate::plugins::youtube_data::apply_youtube_overlay(rows).await;
                let rows = crate::plugins::twitch_live::overlay(rows);
                let rows = crate::plugins::niconico_live::overlay(rows);
                crate::webui::api::filter_holodex_streams(
                    rows,
                    ["UCkamito", "UCnazuna"]
                        .map(str::to_string)
                        .into_iter()
                        .collect(),
                )
            },
        )
        .await;
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["dropped"]);
    }
}
