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
