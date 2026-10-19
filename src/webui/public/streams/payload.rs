//! Explicit viewer payload and command/switchability policy.
use super::images::{channel_avatar_url, thumbnail_for};
use crate::config::Channel;
use crate::plugins::banned_keywords::banned_keyword_hits;
use crate::plugins::stream_item::StreamItem;
use crate::plugins::twitch::channel_id_from_url;
use crate::webui::stream_list::facts::resolve_stream_facts;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// `%转播%` names a platform and a channel, not a video, so two stream items
/// for the same YouTube channel (or the same Twitch login, or the same
/// Niconico program) are one switch target. A unique fallback keeps unkeyed
/// items from grouping together.
fn switch_group_key(stream: &StreamItem) -> String {
    if let Some(live_id) = stream
        .link
        .as_deref()
        .and_then(crate::plugins::live_id_from_link)
    {
        return format!("NC:{live_id}");
    }
    if let Some(login) = stream.link.as_deref().and_then(channel_id_from_url) {
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
    // Keep the first item on equal sort keys, just like the previous stable sort.
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
        } else if let Some(login) = link.and_then(channel_id_from_url) {
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
    stream: &StreamItem,
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
    streams: Vec<StreamItem>,
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

            let facts = resolve_stream_facts(&stream);
            let thumbnail = thumbnail_for(&stream);

            PublicStream {
                id: facts.id,
                title: facts.title,
                topic: facts.topic,
                status: facts.status,
                is_placeholder: facts.is_placeholder,
                placeholder_type: facts.placeholder_type,
                start_scheduled: facts.start_scheduled,
                start_actual: facts.start_actual,
                available_at: facts.available_at,
                published_at: facts.published_at,
                live_viewers: facts.live_viewers,
                channel_name: facts.channel_name,
                channel_photo: facts
                    .channel_photo
                    .or_else(|| channel_avatar_url(&facts.channel_id)),
                thumbnail,
                link: facts.link,
                suggested_area_id: facts.suggested_area_id,
                suggested_area_name: facts.suggested_area_name,
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{build, channel, stream, stream_on};
    use super::*;
    use crate::plugins::youtube_data::{YtLiveDetails, YtSnippet, YtVideo};
    use std::collections::BTreeMap;
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
    /// 20:00 card when Holodex returns the later item first.
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

    /// Holodex does not list Niconico, so overlay items are placeholders keyed
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
    fn serialized_keys_are_the_allowlist() {
        let ordinary = stream("ランク", Some("Gaming"), "UCkamito");
        let mut twitch = ordinary.clone();
        twitch.stream_type = "placeholder".to_string();
        twitch.link = Some("https://twitch.tv/kamito_jp".to_string());
        let mut niconico = ordinary.clone();
        niconico.stream_type = "placeholder".to_string();
        niconico.link = Some("https://live.nicovideo.jp/watch/lv12345".to_string());
        let mut absent = stream("Missing optionals", None, "UCkamito");
        absent.live_viewers = None;
        absent.thumbnail = None;
        let built = build(vec![ordinary, twitch, niconico, absent], true, &[]);
        for item in &built {
            let value = serde_json::to_value(item).unwrap();
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
            assert!(!serde_json::to_string(item).unwrap().contains("UCkamito"));
        }
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

        let items = crate::cluster::with_yt_index_role(
            crate::cluster::YtIndexRole::Peer(std::sync::Arc::new(index)),
            // The channels list's steps (`stream_list::build`).
            async {
                let items = crate::plugins::youtube_discovery::merge_discovered(holodex);
                let items = crate::plugins::youtube_data::apply_youtube_overlay(items).await;
                let items = crate::plugins::twitch_live::overlay(items);
                let items = crate::plugins::niconico_live::overlay(items);
                crate::webui::stream_list::filter_streams(
                    items,
                    ["UCkamito", "UCnazuna"]
                        .map(str::to_string)
                        .into_iter()
                        .collect(),
                )
            },
        )
        .await;
        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, vec!["dropped"]);
    }
    #[test]
    fn each_projection_resolves_current_areas_from_the_same_raw_snapshot() {
        const CHILD: &str = "BILISTREAM_STREAM_LIST_AREA_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "webui::public::streams::payload::tests::each_projection_resolves_current_areas_from_the_same_raw_snapshot", "--nocapture"])
                .env(CHILD, "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // A subprocess gives this test its own unit-store, so no concurrent
        // list/config test observes the temporary areas document.
        let snapshot = crate::webui::stream_list::StreamListSnapshot {
            items: vec![stream("area-contract-keyword", None, "UCexample")],
            holodex_ok_at: None,
            holodex_every: std::time::Duration::from_secs(60),
        };
        for (id, name) in [(101, "First area"), (202, "Second area")] {
            crate::storage::write_json(
                "areas.json",
                &serde_json::json!({
                    "areas": [{"id": id, "name": name, "title_keywords": ["area-contract-keyword"]}]
                }),
            )
            .unwrap();
            let admin = crate::webui::api::admin_stream_list::map_stream_list(&snapshot.items);
            let public = build_public_streams(snapshot.items.clone(), &[], true, &[]);
            assert_eq!(admin[0].suggested_area_id, Some(id));
            assert_eq!(admin[0].suggested_area_name.as_deref(), Some(name));
            assert_eq!(public[0].suggested_area_id, Some(id));
            assert_eq!(public[0].suggested_area_name.as_deref(), Some(name));
        }
    }

    #[test]
    fn twitch_command_group_and_preview_share_canonical_identity() {
        let channels = vec![channel(
            "Example",
            &["ex"],
            Some("UCexample"),
            Some("EXAMPLE_CHANNEL_001"),
        )];
        let link = " HTTPS://WWW.TWITCH.TV/Example_Channel_001/videos?x=1#live ";
        let mut live = stream_on("tw-live", "blocked", None, "UCexample", "live", None);
        live.stream_type = "placeholder".to_string();
        live.link = Some(link.to_string());
        live.thumbnail = None;
        let mut upcoming = live.clone();
        upcoming.id = "tw-next".to_string();
        upcoming.title = "clean".to_string();
        upcoming.status = "upcoming".to_string();
        upcoming.link = Some("http://twitch.tv/example_channel_001".to_string());
        let items = build_public_streams(
            vec![live, upcoming],
            &channels,
            true,
            &["blocked".to_string()],
        );
        assert_eq!(items[0].command_platform.as_deref(), Some("TW"));
        assert_eq!(items[0].command_channel.as_deref(), Some("Example"));
        assert_eq!(items[0].link.as_deref(), Some(link));
        assert_eq!(items[0].thumbnail.as_deref(), Some("https://static-cdn.jtvnw.net/previews-ttv/live_user_example_channel_001-640x360.jpg"));
        assert_eq!(items[1].reason, Some(NotSwitchable::EarlierBannedKeyword));

        let target = ChannelIndex::new(&channels).command_target(
            "UCexample",
            Some("https://example.com/twitch.tv/example_channel_001"),
        );
        assert_eq!(target.unwrap().platform, "YT");
    }
}
