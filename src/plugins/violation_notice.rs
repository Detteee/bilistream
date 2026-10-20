//! The reason-aware 警告/切断 danmaku copy.
//!
//! Owner-confirmed table (PRD `10-20-warn-hold-exempt`): one line per reason
//! class, once for "the stream continues" and once for "the hold stays". The
//! owner asked for no long variant — these lines are the whole copy. They
//! never quote Bilibili's raw 违规原因, so the notice itself cannot trip the
//! danmaku filter.

use std::collections::BTreeSet;

use crate::logging::safe_text;
use crate::plugins::warning_exempt::ReasonId;

/// Code cap; the sending account allows more, so short lines fit any account.
const NOTICE_CAP: usize = 80;

/// The text kept from today: the final fallback when the paused channel's
/// name is unknown, and the second (last) attempt when the chosen line is
/// rejected.
pub(crate) const GENERIC_SHORT: &str = "🚫 警告/切断状态，请换台";

/// Which copy row a classified reason uses. High-risk classes and an
/// unrecognised reason share the generic row (the owner's grouping); a reason
/// matching several specific classes also takes the generic row, because no
/// single short name would be true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeKind {
    IdleRecording,
    GameModeOff,
    Warning,
}

/// Owner text. Held rows carry `{channel_name}`.
struct Copy {
    continues: &'static str,
    held: &'static str,
}

const IDLE: Copy = Copy {
    continues: "🚫挂机判定，稍后自动恢复。",
    held: "🚫挂机判定，暂停转播 {channel_name}，请换台。",
};

const GAME_MODE: Copy = Copy {
    continues: "🚫未开启主播模式判定，稍后自动恢复。",
    held: "🚫未开启主播模式判定，暂停转播 {channel_name}，请换台。",
};

const WARNING: Copy = Copy {
    continues: "🚫 警告/切断状态，稍后自动恢复。",
    held: "🚫 警告/切断状态，暂停转播 {channel_name}，请换台。",
};

impl NoticeKind {
    fn copy(self) -> &'static Copy {
        match self {
            Self::IdleRecording => &IDLE,
            Self::GameModeOff => &GAME_MODE,
            Self::Warning => &WARNING,
        }
    }
}

/// Pick the copy row from the matched classes. High-risk classes always take
/// the generic row; two different specific classes also do.
fn kind(reason_ids: &BTreeSet<ReasonId>) -> NoticeKind {
    let high = reason_ids.contains(&ReasonId::GameInfoViolation)
        || reason_ids.contains(&ReasonId::ContentUnsuitable);
    if high {
        return NoticeKind::Warning;
    }
    let specific: Vec<_> = reason_ids
        .iter()
        .filter(|id| matches!(id, ReasonId::IdleRecording | ReasonId::GameModeOff))
        .collect();
    match specific.as_slice() {
        [ReasonId::IdleRecording] => NoticeKind::IdleRecording,
        [ReasonId::GameModeOff] => NoticeKind::GameModeOff,
        _ => NoticeKind::Warning,
    }
}

/// The one line for this event. `channel_name` is the paused source channel
/// (display only); it is shortened, never the fixed copy, so the line fits
/// [`NOTICE_CAP`].
pub(crate) fn text(reason_ids: &BTreeSet<ReasonId>, released: bool, channel_name: &str) -> String {
    let copy = kind(reason_ids).copy();
    let template = if released { copy.continues } else { copy.held };
    if template.contains("{channel_name}") {
        with_channel(template, channel_name)
    } else {
        template.to_owned()
    }
}

/// Substitute the channel name, shortening only the name to fit the cap.
/// Danmaku length is counted in characters, so this counts characters (after
/// `safe_text` strips control characters and secrets).
fn with_channel(template: &str, channel_name: &str) -> String {
    let name = safe_text(channel_name.trim(), usize::MAX);
    if name.trim().is_empty() {
        return GENERIC_SHORT.to_owned();
    }
    let fixed = template.replace("{channel_name}", "").chars().count();
    let budget = NOTICE_CAP.saturating_sub(fixed);
    let mut chars = name.chars();
    let mut shortened: String = chars.by_ref().take(budget).collect();
    if chars.next().is_some() {
        // Leave one character for the explicit omission marker.
        shortened = name
            .chars()
            .take(budget.saturating_sub(1))
            .collect::<String>();
        shortened.push('…');
    }
    template.replace("{channel_name}", &shortened)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[ReasonId]) -> BTreeSet<ReasonId> {
        ids.iter().copied().collect()
    }

    #[test]
    fn the_owner_lines_are_used_verbatim() {
        let idle = ids(&[ReasonId::IdleRecording]);
        assert_eq!(
            text(&idle, false, "星海频道"),
            "🚫挂机判定，暂停转播 星海频道，请换台。"
        );
        assert_eq!(text(&idle, true, "星海频道"), "🚫挂机判定，稍后自动恢复。");

        let mode = ids(&[ReasonId::GameModeOff]);
        assert_eq!(
            text(&mode, false, "星海频道"),
            "🚫未开启主播模式判定，暂停转播 星海频道，请换台。"
        );
        assert_eq!(
            text(&mode, true, "星海频道"),
            "🚫未开启主播模式判定，稍后自动恢复。"
        );

        for high in [
            ids(&[ReasonId::GameInfoViolation]),
            ids(&[ReasonId::ContentUnsuitable]),
            ids(&[]),
        ] {
            assert_eq!(
                text(&high, false, "星海频道"),
                "🚫 警告/切断状态，暂停转播 星海频道，请换台。"
            );
        }
        // Only a selectable high-tag reason reaches this row's continues line;
        // an unknown reason never continues.
        assert_eq!(
            text(&ids(&[ReasonId::ContentUnsuitable]), true, "星海频道"),
            "🚫 警告/切断状态，稍后自动恢复。"
        );
    }

    #[test]
    fn every_line_fits_the_cap() {
        let rows = [
            (ids(&[ReasonId::IdleRecording]), "星海频道"),
            (ids(&[ReasonId::GameModeOff]), "星海频道"),
            (ids(&[ReasonId::GameInfoViolation]), "星海频道"),
            (BTreeSet::new(), "星海频道"),
        ];
        for (ids, name) in rows {
            for released in [false, true] {
                let line = text(&ids, released, name);
                assert!(line.chars().count() <= NOTICE_CAP, "{line}");
            }
        }
    }

    #[test]
    fn an_unknown_channel_falls_back_to_todays_generic_text() {
        assert_eq!(
            text(&ids(&[ReasonId::IdleRecording]), false, "  "),
            GENERIC_SHORT
        );
    }

    #[test]
    fn a_long_channel_name_is_shortened_so_the_copy_fits() {
        let line = text(
            &ids(&[ReasonId::IdleRecording]),
            false,
            &"很长的频道名字".repeat(20),
        );
        assert_eq!(line.chars().count(), NOTICE_CAP, "{line}");
        assert!(line.starts_with("🚫挂机判定，暂停转播 "), "{line}");
        assert!(line.ends_with("，请换台。"), "{line}");
        assert!(line.contains('…'), "only the name is shortened: {line}");
    }

    #[test]
    fn high_risk_and_mixed_specific_classes_take_the_generic_row() {
        assert_eq!(
            kind(&ids(&[ReasonId::GameInfoViolation])),
            NoticeKind::Warning
        );
        assert_eq!(
            kind(&ids(&[ReasonId::ContentUnsuitable])),
            NoticeKind::Warning
        );
        assert_eq!(
            kind(&ids(&[
                ReasonId::IdleRecording,
                ReasonId::GameInfoViolation
            ])),
            NoticeKind::Warning,
            "high wins the copy row"
        );
        assert_eq!(
            kind(&ids(&[ReasonId::IdleRecording, ReasonId::GameModeOff])),
            NoticeKind::Warning,
            "two specific classes have no single short name"
        );
        assert_eq!(
            kind(&ids(&[ReasonId::IdleRecording])),
            NoticeKind::IdleRecording
        );
        assert_eq!(
            kind(&ids(&[ReasonId::GameModeOff])),
            NoticeKind::GameModeOff
        );
        assert_eq!(kind(&BTreeSet::new()), NoticeKind::Warning);
    }

    #[test]
    fn the_copy_never_quotes_the_reason_or_the_filter_words() {
        let rows = [
            ids(&[ReasonId::IdleRecording]),
            ids(&[ReasonId::GameModeOff]),
            ids(&[ReasonId::GameInfoViolation]),
            ids(&[ReasonId::ContentUnsuitable]),
            BTreeSet::new(),
        ];
        for ids in rows {
            for released in [false, true] {
                let line = text(&ids, released, "星海频道");
                for never in ["违规信息", "不适宜", "违规原因"] {
                    assert!(!line.contains(never), "{never} leaked into: {line}");
                }
            }
        }
    }
}
