//! Built-in 违规原因 classification for the warning-hold exemption
//! (PRD `10-20-warn-hold-exempt`). Pure data and matching only: the config,
//! the cluster sync and the hold decision live in their own slices.
//!
//! The input is the cleaned 违规原因 text (control characters stripped,
//! whitespace normalised), never the 处罚结果. Matching is plain substring
//! fragments, because Bilibili varies the sentence around them. A reason that
//! matches several classes releases only when every matched class is ticked,
//! so [`classify`] returns the whole set; no match means unknown, which can
//! never release a hold.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The built-in reason classes, in the order the panel lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonId {
    /// 挂机 / 录播等消极直播.
    IdleRecording,
    /// 未开启该游戏直播模式.
    GameModeOff,
    /// 游戏中存在违规信息.
    GameInfoViolation,
    /// 直播内容不适宜.
    ContentUnsuitable,
}

impl ReasonId {
    pub const ALL: [ReasonId; 4] = [
        Self::IdleRecording,
        Self::GameModeOff,
        Self::GameInfoViolation,
        Self::ContentUnsuitable,
    ];

    /// Stable setting id; stored in config, never the phrase text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdleRecording => "idle_recording",
            Self::GameModeOff => "game_mode_off",
            Self::GameInfoViolation => "game_info_violation",
            Self::ContentUnsuitable => "content_unsuitable",
        }
    }

    /// Parse a stored setting id; unknown ids are refused by the API.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.as_str() == value)
    }

    /// Display tag only; every class is selectable.
    pub fn risk(self) -> RiskTier {
        match self {
            Self::IdleRecording => RiskTier::Low,
            Self::GameModeOff => RiskTier::Medium,
            Self::GameInfoViolation | Self::ContentUnsuitable => RiskTier::High,
        }
    }
}

/// The panel's risk tag. Guidance for the owner, not a matching rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTier {
    Low,
    Medium,
    High,
}

impl RiskTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "低风险",
            Self::Medium => "中风险",
            Self::High => "高风险",
        }
    }
}

/// A monitored channel identity exempted from the warning/cut hold. Stored as
/// the resolved (platform, channel id) pairs the hold matches, so a release
/// check stays a plain list scan; `name` is display-only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemptChannel {
    pub platform: String,
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

impl ExemptChannel {
    /// Canonical platform code and channel id, or `None` when the platform is
    /// not relayed or the id is blank. The platform codes are the same YT/TW/NC
    /// the hold records, and TW ids are lowercased like the hold's.
    pub fn new(platform: &str, channel_id: &str, name: &str) -> Option<Self> {
        let platform = match platform.trim().to_ascii_uppercase().as_str() {
            "YT" => "YT",
            "TW" => "TW",
            "NC" => "NC",
            _ => return None,
        };
        let channel_id = super::danmaku::warning::canonical_channel_id(platform, channel_id);
        if channel_id.is_empty() {
            return None;
        }
        Some(Self {
            platform: platform.to_owned(),
            channel_id,
            name: name.trim().to_owned(),
        })
    }

    /// Whether this entry covers the held source identity.
    pub fn matches(&self, platform: &str, channel_id: &str) -> bool {
        self.platform == platform
            && self.channel_id
                == super::danmaku::warning::canonical_channel_id(platform, channel_id)
    }
}

/// Split stored reason ids into built-in ones and unknown ones, deduplicating
/// and preserving order. A peer push keeps the first list; the settings API
/// refuses when the second is non-empty.
pub fn split_reason_ids(ids: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    for id in ids {
        if ReasonId::parse(&id).is_none() {
            if !unknown.contains(&id) {
                unknown.push(id);
            }
        } else if !known.contains(&id) {
            known.push(id);
        }
    }
    (known, unknown)
}

/// One roster channel's canonical platform identities (display name included).
pub fn channel_identities(channel: &crate::config::Channel) -> Vec<ExemptChannel> {
    let mut out = Vec::new();
    for (platform, id) in [
        ("YT", channel.platforms.youtube.as_deref()),
        ("TW", channel.platforms.twitch.as_deref()),
        ("NC", channel.platforms.niconico.as_deref()),
    ] {
        if let Some(entry) = id.and_then(|id| ExemptChannel::new(platform, id, &channel.name)) {
            out.push(entry);
        }
    }
    out
}

/// Every platform identity in a roster document.
pub fn roster_identities(roster: &crate::config::ChannelsData) -> Vec<ExemptChannel> {
    roster
        .channels
        .iter()
        .flat_map(channel_identities)
        .collect()
}

/// Drop exempt entries whose identity is absent from a pushed channels
/// document. `None` (a peer that sent no roster) keeps the list untouched.
pub fn retain_known_channels(
    entries: &mut Vec<ExemptChannel>,
    channels: Option<&serde_json::Value>,
) {
    let Some(document) = channels else {
        return;
    };
    // The pushed document is shape-validated before it is applied; when it
    // cannot be read as a roster, keep the list (it cannot prove an identity
    // is gone).
    let Ok(roster) = serde_json::from_value::<crate::config::ChannelsData>(document.clone()) else {
        return;
    };
    let known: BTreeSet<_> = roster_identities(&roster)
        .into_iter()
        .map(|entry| (entry.platform, entry.channel_id))
        .collect();
    entries.retain(|entry| known.contains(&(entry.platform.clone(), entry.channel_id.clone())));
}

/// At most one automatic restart per exempt channel and episode inside this
/// window after a hold-worthy event (PRD R4); a later event holds as today.
pub const EXEMPT_RESTART_WINDOW_SECS: u64 = 30 * 60;

/// Defensive cap on the synced rail (one entry per channel normally).
pub const MAX_EXEMPT_RESTARTS: usize = 16;

/// The last exempt restart of one channel, shared through the signed
/// heartbeat so a failover node makes the same R4 call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemptRestart {
    pub platform: String,
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<String>,
    /// Unix seconds when the hold was released.
    pub at: u64,
    /// The matched reason id that allowed it (diagnostics only).
    pub reason: String,
}

/// Per-node list of exempt restarts, one entry per channel. Entries expire
/// after [`EXEMPT_RESTART_WINDOW_SECS`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemptRail {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restarts: Vec<ExemptRestart>,
}

impl ExemptRail {
    /// Drop entries older than the R4 window.
    pub fn prune(&mut self, now: u64) {
        self.restarts
            .retain(|restart| restart.at.saturating_add(EXEMPT_RESTART_WINDOW_SECS) > now);
    }

    /// Whether this channel already used its one exempt restart for the
    /// episode. An unknown episode on either side matches conservatively;
    /// two known different episodes are separate episodes.
    pub fn hit(
        &self,
        platform: &str,
        channel_id: &str,
        episode_id: Option<&str>,
        now: u64,
    ) -> bool {
        let Some(identity) = ExemptChannel::new(platform, channel_id, "") else {
            return false;
        };
        self.restarts.iter().any(|restart| {
            restart.platform == identity.platform
                && restart.channel_id == identity.channel_id
                && restart.at.saturating_add(EXEMPT_RESTART_WINDOW_SECS) > now
                && match (restart.episode_id.as_deref(), episode_id) {
                    (Some(recorded), Some(current)) => recorded == current,
                    _ => true,
                }
        })
    }

    /// Record the one allowed automatic restart of this channel+episode.
    /// Replaces any older entry for the channel (at most one) and caps the
    /// list defensively.
    pub fn record(
        &mut self,
        platform: &str,
        channel_id: &str,
        episode_id: Option<&str>,
        reason: &str,
        now: u64,
    ) {
        self.prune(now);
        let Some(identity) = ExemptChannel::new(platform, channel_id, "") else {
            return;
        };
        self.restarts.retain(|restart| {
            !(restart.platform == identity.platform && restart.channel_id == identity.channel_id)
        });
        if self.restarts.len() >= MAX_EXEMPT_RESTARTS {
            self.restarts.remove(0);
        }
        self.restarts.push(ExemptRestart {
            platform: identity.platform,
            channel_id: identity.channel_id,
            episode_id: episode_id.map(str::to_owned),
            at: now,
            reason: reason.to_owned(),
        });
    }
}

/// The node-local document that keeps the rail across restarts.
pub const RAIL_DOCUMENT: &str = "warning_exempt_rail.json";

/// This node's persisted rail (empty before the first exempt restart).
pub fn local_rail() -> ExemptRail {
    crate::storage::read_document(RAIL_DOCUMENT).unwrap_or_default()
}

/// Whether a recent exempt restart of this channel+episode exists on this
/// node or on a peer (the rail travels in the signed heartbeat).
pub fn rail_hit(platform: &str, channel_id: &str, episode_id: Option<&str>, now: u64) -> bool {
    local_rail().hit(platform, channel_id, episode_id, now)
        || crate::cluster::peer_exempt_rails_hit(platform, channel_id, episode_id, now)
}

/// Persist the one allowed exempt restart, replacing this channel's entry.
pub fn record_restart(
    platform: &str,
    channel_id: &str,
    episode_id: Option<&str>,
    reason: &str,
    now: u64,
) -> Result<(), String> {
    let mut rail = local_rail();
    rail.record(platform, channel_id, episode_id, reason, now);
    crate::storage::write_document(RAIL_DOCUMENT, &rail)
        .map(|_| ())
        .map_err(|_| "警告豁免记录保存失败".to_string())
}

/// The heartbeat view: an empty rail is omitted from the payload.
pub fn heartbeat_rail() -> Option<ExemptRail> {
    let rail = local_rail();
    (!rail.restarts.is_empty()).then_some(rail)
}

/// The exemption decision for one held source. Pure; the caller supplies the
/// configured list/reasons, the matched classes, the R4 rail result and
/// whether this is a lock rather than a per-source warning hold.
pub struct HoldDecision<'a> {
    pub exempt: &'a [ExemptChannel],
    /// Ticked built-in reason ids.
    pub allowed_reasons: &'a [String],
    /// Low-risk reasons apply to every monitored channel.
    pub all_low_risk: bool,
    pub platform: &'a str,
    pub channel_id: &'a str,
    /// Every class the held reason matched; empty means unknown.
    pub reason_ids: &'a BTreeSet<ReasonId>,
    /// The rail already used this channel+episode's one restart.
    pub rail_hit: bool,
    /// ROOM_LOCK (or any non-per-source hold) is never exempt.
    pub room_lock: bool,
}

impl HoldDecision<'_> {
    /// Cheap conditions that need no I/O: not a lock, a known reason, every
    /// matched class ticked, and either the held identity is on the exempt
    /// list or every matched class is low risk under the global switch.
    pub fn configured_release(&self) -> bool {
        let allowed = !self.reason_ids.is_empty()
            && self.reason_ids.iter().all(|id| {
                self.allowed_reasons
                    .iter()
                    .any(|allowed| allowed == id.as_str())
            });
        let listed = self
            .exempt
            .iter()
            .any(|entry| entry.matches(self.platform, self.channel_id));
        let low_risk_everywhere =
            self.all_low_risk && self.reason_ids.iter().all(|id| id.risk() == RiskTier::Low);
        !self.room_lock && allowed && (listed || low_risk_everywhere)
    }

    /// Release only when the cheap conditions pass and the R4 rail has room.
    pub fn releases(&self) -> bool {
        self.configured_release() && !self.rail_hit
    }
}

/// Every built-in class the cleaned 违规原因 matches, in one pass. Empty means
/// unknown (or a missing reason / the mask), which never releases a hold.
pub fn classify(reason: &str) -> BTreeSet<ReasonId> {
    let mut ids = BTreeSet::new();
    // High-tag rules first in the PRD table; the set keeps every match so a
    // reason touching two classes needs both ticked.
    if reason.contains("违规信息") {
        ids.insert(ReasonId::GameInfoViolation);
    }
    if reason.contains("直播内容不适宜") {
        ids.insert(ReasonId::ContentUnsuitable);
    }
    if reason.contains("未开启该游戏直播模式") {
        ids.insert(ReasonId::GameModeOff);
    }
    if (reason.contains("挂机") || reason.contains("录播")) && reason.contains("消极直播") {
        ids.insert(ReasonId::IdleRecording);
    }
    ids
}

/// The highest risk tag of a classification; unknown (empty) reads as high and
/// therefore never releases.
pub fn highest_risk(ids: &BTreeSet<ReasonId>) -> RiskTier {
    ids.iter()
        .map(|id| id.risk())
        .max()
        .unwrap_or(RiskTier::High)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(reason: &str) -> Vec<&'static str> {
        classify(reason).into_iter().map(ReasonId::as_str).collect()
    }

    #[test]
    fn the_three_idle_variants_are_low_risk() {
        for reason in [
            "您本场直播存在挂机、录播等消极直播行为，因此直播被切断，请您及时整改",
            "您本场直播存在挂机、录播等消极直播行为，因此直播被切断，请您及时整改（切断本场直播）",
            "您近期直播存在挂机、录播等消极直播行为，请及时整改",
            "存在挂机、录播等消极直播行为，并取消未来24小时直播推荐资格",
        ] {
            assert_eq!(ids(reason), vec!["idle_recording"], "{reason}");
            assert_eq!(highest_risk(&classify(reason)), RiskTier::Low, "{reason}");
        }
    }

    #[test]
    fn game_mode_off_is_medium() {
        assert_eq!(ids("未开启该游戏直播模式"), vec!["game_mode_off"]);
        assert_eq!(
            highest_risk(&classify("未开启该游戏直播模式")),
            RiskTier::Medium
        );
    }

    #[test]
    fn the_high_risk_classes_are_distinct_and_matched_by_fragment() {
        assert_eq!(
            ids("游戏中玩家语聊、昵称等存在违规信息"),
            vec!["game_info_violation"]
        );
        assert_eq!(ids("直播内容不适宜"), vec!["content_unsuitable"]);
        assert_eq!(highest_risk(&classify("存在违规信息")), RiskTier::High);
        assert_eq!(highest_risk(&classify("直播内容不适宜")), RiskTier::High);
    }

    #[test]
    fn every_matched_class_is_returned_not_just_the_highest() {
        // 挂机+消极直播 (low) and 违规信息 (high): both must be ticked.
        let ids = classify("存在挂机等消极直播行为，且含有违规信息");
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            vec![ReasonId::IdleRecording, ReasonId::GameInfoViolation]
        );
        assert_eq!(
            highest_risk(&classify("存在挂机等消极直播行为，且含有违规信息")),
            RiskTier::High
        );
    }

    #[test]
    fn unknown_empty_and_stock_text_match_nothing() {
        for reason in [
            "警告/切断状态，请换台",
            "",
            "直播间警告",
            "图片内容不适宜，请立即调整",
        ] {
            assert!(classify(reason).is_empty(), "{reason}");
            assert_eq!(highest_risk(&classify(reason)), RiskTier::High, "{reason}");
        }
        // Partial rules don't match: 挂机 or 录播 alone, 消极直播 alone.
        for reason in ["挂机", "录播", "消极直播", "未开启直播模式"] {
            assert!(classify(reason).is_empty(), "{reason}");
        }
    }

    #[test]
    fn exempt_entries_store_canonical_identity_and_match_the_hold() {
        let entry = ExemptChannel::new(" tw ", "StarSea", " 星海频道 ").unwrap();
        assert_eq!(entry.platform, "TW");
        assert_eq!(entry.channel_id, "starsea");
        assert_eq!(entry.name, "星海频道");
        assert!(entry.matches("TW", "STARSEA"));
        assert!(!entry.matches("YT", "starsea"));
        assert!(!entry.matches("TW", "other"));
        // Unknown platforms and blank ids are not identities.
        assert!(ExemptChannel::new("BILI", "x", "").is_none());
        assert!(ExemptChannel::new("YT", "  ", "").is_none());
    }

    #[test]
    fn pushed_reasons_keep_known_ids_once_and_report_unknown_ones() {
        assert_eq!(
            split_reason_ids([
                "idle_recording".to_string(),
                "not_a_reason".to_string(),
                "game_mode_off".to_string(),
                "idle_recording".to_string(),
            ]),
            (
                vec!["idle_recording".to_string(), "game_mode_off".to_string()],
                vec!["not_a_reason".to_string()]
            )
        );
        assert_eq!(split_reason_ids(Vec::new()), (Vec::new(), Vec::new()));
    }

    #[test]
    fn a_pushed_roster_drops_identities_it_no_longer_has() {
        let roster = serde_json::json!({
            "channels": [
                { "name": "星海频道", "aliases": [], "platforms": { "youtube": "UC1", "twitch": "StarSea" } }
            ]
        });
        let mut entries = vec![
            ExemptChannel::new("YT", "UC1", "星海频道").unwrap(),
            ExemptChannel::new("TW", "starsea", "星海频道").unwrap(),
            ExemptChannel::new("NC", "ch2648162", "其他").unwrap(),
        ];
        retain_known_channels(&mut entries, Some(&roster));
        assert_eq!(
            entries
                .iter()
                .map(|e| e.platform.as_str())
                .collect::<Vec<_>>(),
            vec!["YT", "TW"]
        );
        // A peer that sent no roster cannot prove an identity is gone.
        let mut untouched = entries.clone();
        retain_known_channels(&mut untouched, None);
        assert_eq!(untouched, entries);
    }

    #[test]
    fn the_rail_allows_one_restart_per_channel_episode_within_thirty_minutes() {
        let mut rail = ExemptRail::default();
        let now = 1_000;
        rail.record("TW", "starsea", Some("ep-1"), "idle_recording", now);
        assert!(rail.hit("TW", "STARSEA", Some("ep-1"), now + 1));
        assert!(
            !rail.hit("TW", "starsea", Some("ep-2"), now + 1),
            "another episode"
        );
        assert!(
            rail.hit("TW", "starsea", None, now + 1),
            "an unknown episode matches conservatively"
        );
        assert!(!rail.hit("YT", "starsea", Some("ep-1"), now + 1));
        assert!(
            rail.hit(
                "TW",
                "starsea",
                Some("ep-1"),
                now + EXEMPT_RESTART_WINDOW_SECS - 1
            ),
            "inside the window"
        );
        assert!(
            !rail.hit(
                "TW",
                "starsea",
                Some("ep-1"),
                now + EXEMPT_RESTART_WINDOW_SECS
            ),
            "expired at the edge"
        );

        // A second restart replaces the channel's entry; other channels add.
        rail.record("TW", "starsea", Some("ep-2"), "game_mode_off", now + 10);
        assert_eq!(rail.restarts.len(), 1);
        assert!(!rail.hit("TW", "starsea", Some("ep-1"), now + 11));
        assert!(rail.hit("TW", "starsea", Some("ep-2"), now + 11));
        rail.record("YT", "UC1", None, "idle_recording", now);
        assert_eq!(rail.restarts.len(), 2);
        rail.prune(now + EXEMPT_RESTART_WINDOW_SECS + 100);
        assert!(rail.restarts.is_empty());
    }

    #[test]
    fn the_decision_needs_a_known_ticked_reason_an_exempt_channel_and_rail_room() {
        let exempt = vec![ExemptChannel::new("YT", "UC1", "星海频道").unwrap()];
        let allowed = vec!["idle_recording".to_string(), "game_mode_off".to_string()];
        let idle: BTreeSet<ReasonId> = [ReasonId::IdleRecording].into_iter().collect();
        let both: BTreeSet<ReasonId> = [ReasonId::IdleRecording, ReasonId::GameInfoViolation]
            .into_iter()
            .collect();
        let unknown = BTreeSet::new();
        fn decision<'a>(
            exempt: &'a [ExemptChannel],
            allowed: &'a [String],
            ids: &'a BTreeSet<ReasonId>,
            rail_hit: bool,
            room_lock: bool,
            channel_id: &'a str,
        ) -> HoldDecision<'a> {
            HoldDecision {
                exempt,
                allowed_reasons: allowed,
                all_low_risk: false,
                platform: "YT",
                channel_id,
                reason_ids: ids,
                rail_hit,
                room_lock,
            }
        }
        assert!(decision(&exempt, &allowed, &idle, false, false, "UC1").releases());
        assert!(
            !decision(&exempt, &allowed, &unknown, false, false, "UC1").releases(),
            "unknown never releases"
        );
        assert!(
            !decision(&exempt, &allowed, &both, false, false, "UC1").releases(),
            "every matched class must be ticked"
        );
        assert!(
            !decision(&exempt, &allowed, &idle, true, false, "UC1").releases(),
            "rail hit"
        );
        assert!(
            !decision(&exempt, &allowed, &idle, false, true, "UC1").releases(),
            "lock"
        );
        assert!(
            !decision(&exempt, &allowed, &idle, false, false, "UC2").releases(),
            "not exempt"
        );

        // The global low-risk switch releases an unlisted channel when every
        // matched class is low risk; a mid/high class still needs the list.
        fn global<'a>(
            exempt: &'a [ExemptChannel],
            allowed: &'a [String],
            channel_id: &'a str,
            ids: &'a BTreeSet<ReasonId>,
        ) -> HoldDecision<'a> {
            HoldDecision {
                exempt,
                allowed_reasons: allowed,
                all_low_risk: true,
                platform: "YT",
                channel_id,
                reason_ids: ids,
                rail_hit: false,
                room_lock: false,
            }
        }
        assert!(
            global(&exempt, &allowed, "UC2", &idle).releases(),
            "global low-risk idle"
        );
        assert!(
            !global(&exempt, &allowed, "UC2", &both).releases(),
            "a high-tag class still needs the channel list"
        );
        assert!(
            !global(&exempt, &allowed, "UC2", &BTreeSet::new()).releases(),
            "unknown never releases"
        );
        let mid: BTreeSet<ReasonId> = [ReasonId::GameModeOff].into_iter().collect();
        assert!(
            !global(&exempt, &allowed, "UC2", &mid).releases(),
            "the mid-risk class is not covered by the low-risk switch"
        );
        let high_ticked = vec![
            "idle_recording".to_string(),
            "game_info_violation".to_string(),
        ];
        let listed_high = HoldDecision {
            exempt: &exempt,
            allowed_reasons: &high_ticked,
            all_low_risk: true,
            platform: "YT",
            channel_id: "UC1",
            reason_ids: &both,
            rail_hit: false,
            room_lock: false,
        };
        assert!(listed_high.releases(), "a listed channel covers mid/high");
    }

    #[test]
    fn ids_round_trip_and_unknown_ids_are_rejected() {
        for id in ReasonId::ALL {
            assert_eq!(ReasonId::parse(id.as_str()), Some(id));
            assert_eq!(
                serde_json::to_value(id).unwrap(),
                serde_json::json!(id.as_str())
            );
            assert_eq!(
                serde_json::from_value::<ReasonId>(serde_json::json!(id.as_str())).unwrap(),
                id
            );
        }
        assert_eq!(ReasonId::parse("afk_replay"), None);
        assert_eq!(ReasonId::parse("IDLE_RECORDING"), None);
        assert_eq!(ReasonId::parse(""), None);
    }
}
