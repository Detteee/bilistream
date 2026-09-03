//! Banned keyword lists from `areas.json`.
//!
//! Two independent lists live there:
//!
//! * `banned_keywords` blocks a danmaku `%转播%` request, and
//! * `streaming_banned_keywords` makes the monitor skip a live stream.
//!
//! The danmaku predicate is shared with the public status page, which greys out
//! a stream it would reject, so both must agree on what counts as a hit.

use serde_json::Value;

fn read_areas_json() -> Option<Value> {
    let areas_path = match std::env::current_exe() {
        Ok(path) => path.with_file_name("areas.json"),
        Err(e) => {
            tracing::error!("无法获取可执行文件路径: {}", e);
            return None;
        }
    };

    let content = match std::fs::read_to_string(&areas_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("无法读取 areas.json: {}", e);
            return None;
        }
    };

    match serde_json::from_str(&content) {
        Ok(data) => Some(data),
        Err(e) => {
            tracing::error!("无法解析 areas.json: {}", e);
            None
        }
    }
}

fn read_keyword_list(data: &Value, key: &str) -> Option<Vec<String>> {
    data[key].as_array().map(|keywords| {
        keywords
            .iter()
            .filter_map(|k| k.as_str().map(|s| s.to_string()))
            .collect()
    })
}

/// Keywords that block a danmaku `%转播%` request.
pub fn danmaku_banned_keywords() -> Vec<String> {
    let Some(data) = read_areas_json() else {
        return Vec::new();
    };

    read_keyword_list(&data, "banned_keywords").unwrap_or_else(|| {
        tracing::warn!("areas.json 中未找到 banned_keywords");
        Vec::new()
    })
}

/// Keywords that make the monitor skip a live stream.
pub fn streaming_banned_keywords() -> Vec<String> {
    let Some(data) = read_areas_json() else {
        return Vec::new();
    };

    read_keyword_list(&data, "streaming_banned_keywords").unwrap_or_else(|| {
        tracing::warn!("areas.json 中未找到 streaming_banned_keywords，使用默认值");
        default_streaming_banned_keywords()
    })
}

fn default_streaming_banned_keywords() -> Vec<String> {
    [
        "どうぶつの森",
        "animal crossing",
        "asmr",
        "dbd",
        "dead by daylight",
        "l4d2",
        "left 4 dead 2",
        "gta",
        "mad town",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Topic and title joined the way the monitor skip-check does it.
///
/// Unlike [`danmaku_haystack`], this keeps the original case: the streaming
/// skip has always been a plain `contains` on the concatenated fields.
pub fn streaming_haystack(topic: Option<&str>, title: Option<&str>) -> Option<String> {
    match (topic, title) {
        (Some(topic), Some(title)) => Some(format!("{topic} {title}")),
        _ => title.map(str::to_string),
    }
}

/// First keyword in `keywords` contained in the monitor haystack, if any.
///
/// The haystack is lowercased so `areas.json` can keep English keywords in
/// lowercase and still hit titles like `ASMR` or topics like `Just Chatting`.
pub fn streaming_banned_hit(
    topic: Option<&str>,
    title: Option<&str>,
    keywords: &[String],
) -> Option<String> {
    banned_keyword_hit(&streaming_haystack(topic, title)?.to_lowercase(), keywords)
}

/// The haystack a danmaku request is matched against: topic and title joined
/// and lowercased, so keywords only need their lowercase form.
pub fn danmaku_haystack(topic: &str, title: &str) -> String {
    format!("{} {}", topic, title).to_lowercase()
}

/// First keyword in `keywords` contained in `haystack`, if any.
pub fn banned_keyword_hit(haystack: &str, keywords: &[String]) -> Option<String> {
    keywords
        .iter()
        .find(|keyword| haystack.contains(keyword.as_str()))
        .cloned()
}

/// Whether a danmaku request for this topic/title would be rejected, and on
/// which keyword. Loads the list, so callers checking many streams at once
/// should use [`danmaku_banned_keywords`] with [`banned_keyword_hit`] instead.
pub fn danmaku_banned_hit(topic: &str, title: &str) -> Option<String> {
    banned_keyword_hit(&danmaku_haystack(topic, title), &danmaku_banned_keywords())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keywords(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn streaming_haystack_joins_topic_and_title() {
        assert_eq!(
            streaming_haystack(Some("Just Chatting"), Some("雑談")).as_deref(),
            Some("Just Chatting 雑談")
        );
    }

    #[test]
    fn streaming_haystack_falls_back_to_title_when_topic_is_missing() {
        assert_eq!(
            streaming_haystack(None, Some("ランク")).as_deref(),
            Some("ランク")
        );
        assert_eq!(streaming_haystack(Some("Just Chatting"), None), None);
        assert_eq!(streaming_haystack(None, None), None);
    }

    #[test]
    fn streaming_hit_matches_keyword_in_title_or_joined_topic() {
        let list = keywords(&["asmr", "just chatting"]);
        assert_eq!(
            streaming_banned_hit(None, Some("late night asmr"), &list).as_deref(),
            Some("asmr")
        );
        assert_eq!(
            streaming_banned_hit(Some("Just Chatting"), Some("雑談"), &list).as_deref(),
            Some("just chatting")
        );
        assert_eq!(
            streaming_banned_hit(None, Some("【ASMR】睡眠導入"), &list).as_deref(),
            Some("asmr")
        );
        assert!(streaming_banned_hit(None, Some("ランク"), &list).is_none());
        assert!(streaming_banned_hit(None, None, &list).is_none());
    }

    #[test]
    fn hit_matches_keyword_in_title() {
        let list = keywords(&["asmr", "gta"]);
        let haystack = danmaku_haystack("Just Chatting", "Late night ASMR");
        assert_eq!(
            banned_keyword_hit(&haystack, &list).as_deref(),
            Some("asmr")
        );
    }

    #[test]
    fn hit_matches_keyword_in_topic() {
        let list = keywords(&["dead by daylight"]);
        let haystack = danmaku_haystack("Dead by Daylight", "配信");
        assert_eq!(
            banned_keyword_hit(&haystack, &list).as_deref(),
            Some("dead by daylight")
        );
    }

    #[test]
    fn hit_returns_the_first_matching_keyword() {
        let list = keywords(&["gta", "asmr"]);
        let haystack = danmaku_haystack("", "asmr and gta");
        assert_eq!(banned_keyword_hit(&haystack, &list).as_deref(), Some("gta"));
    }

    #[test]
    fn clean_stream_has_no_hit() {
        let list = keywords(&["asmr"]);
        let haystack = danmaku_haystack("League of Legends", "ランク");
        assert!(banned_keyword_hit(&haystack, &list).is_none());
    }

    #[test]
    fn empty_keyword_list_never_matches() {
        let haystack = danmaku_haystack("Gaming", "anything");
        assert!(banned_keyword_hit(&haystack, &[]).is_none());
    }

    #[test]
    fn keyword_list_reads_string_entries_only() {
        let data = serde_json::json!({ "banned_keywords": ["asmr", 42, "gta"] });
        assert_eq!(
            read_keyword_list(&data, "banned_keywords"),
            Some(keywords(&["asmr", "gta"]))
        );
    }

    #[test]
    fn missing_key_is_distinct_from_an_empty_list() {
        let data = serde_json::json!({ "banned_keywords": [] });
        assert_eq!(
            read_keyword_list(&data, "banned_keywords"),
            Some(Vec::new())
        );
        assert_eq!(read_keyword_list(&data, "streaming_banned_keywords"), None);
    }
}
