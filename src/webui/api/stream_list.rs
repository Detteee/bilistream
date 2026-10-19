//! Admin projection and queries for the multi-provider stream list.
use crate::plugins::stream_item::StreamItem;
use crate::webui::stream_list::facts::resolve_stream_facts;
use axum::{extract::Query, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

// Multi-provider live/upcoming items (the route name stays compatibility vocabulary).
#[derive(Serialize, Debug)]
pub struct AdminStreamListItem {
    pub id: String,
    pub title: String,
    pub topic: Option<String>,
    pub status: String,
    pub start_scheduled: Option<String>,
    pub start_actual: Option<String>,
    pub available_at: Option<String>,
    pub published_at: Option<String>,
    pub live_viewers: Option<i32>,
    pub channel_id: String,
    pub channel_name: String,
    pub channel_photo: Option<String>,
    pub suggested_area_id: Option<u64>,
    pub suggested_area_name: Option<String>,
    pub is_placeholder: bool,
    pub placeholder_type: Option<String>,
    pub link: Option<String>,
    pub thumbnail: Option<String>,
}

pub(crate) fn map_stream_list(streams: &[StreamItem]) -> Vec<AdminStreamListItem> {
    streams
        .iter()
        .map(|stream| {
            let facts = resolve_stream_facts(stream);
            AdminStreamListItem {
                id: facts.id,
                title: facts.title,
                topic: facts.topic,
                status: facts.status,
                start_scheduled: facts.start_scheduled,
                start_actual: facts.start_actual,
                available_at: facts.available_at,
                published_at: facts.published_at,
                live_viewers: facts.live_viewers,
                channel_id: facts.channel_id,
                channel_name: facts.channel_name,
                channel_photo: facts.channel_photo,
                suggested_area_id: facts.suggested_area_id,
                suggested_area_name: facts.suggested_area_name,
                is_placeholder: facts.is_placeholder,
                placeholder_type: facts.placeholder_type,
                link: facts.link,
                thumbnail: facts.thumbnail,
            }
        })
        .collect()
}

#[derive(Deserialize)]
pub struct StreamListQuery {
    /// When true, fetch account favorites (requires JWT). Otherwise uses channels.json.
    #[serde(default)]
    favorites: bool,
    /// When true, fetch Holodex now instead of waiting for its cadence.
    #[serde(default)]
    force: bool,
}

pub async fn get_stream_list(Query(query): Query<StreamListQuery>) -> Json<serde_json::Value> {
    use crate::webui::stream_list::{current, ListKind};
    let kind = if query.favorites {
        ListKind::Favorites
    } else {
        ListKind::Channels
    };
    match current(kind, query.force).await {
        Ok(snapshot) => Json(json!({
            "success": true,
            "source": kind.source(),
            "data": map_stream_list(&snapshot.items)
        })),
        Err(message) => Json(json!({ "success": false, "message": message })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::stream_item::{StreamChannel, StreamItem};
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

    #[test]
    fn admin_rows_serialize_link_topic_and_is_placeholder() {
        let mut ordinary = stream(
            "yt1",
            "live",
            Some("2026-10-03T01:00:00Z"),
            Some("2026-10-03T01:05:00Z"),
        );
        ordinary.topic_id = Some("minecraft".to_string());
        ordinary.link = Some("https://www.youtube.com/watch?v=yt1".to_string());
        ordinary.channel.id = "UCordinary".to_string();

        let mut placeholder = stream("ph1", "live", None, None);
        placeholder.stream_type = "placeholder".to_string();
        placeholder.topic_id = Some("GTA".to_string());
        placeholder.link = Some("https://www.twitch.tv/example".to_string());
        placeholder.channel.id = "UCplaceholder".to_string();

        let mut niconico = stream("nc1", "upcoming", None, None);
        niconico.stream_type = "placeholder".to_string();
        niconico.link = Some("https://live.nicovideo.jp/watch/lv12345".to_string());
        let missing = stream("missing", "upcoming", None, None);
        let sources = [ordinary, placeholder, niconico, missing];
        let items = map_stream_list(&sources);
        assert_eq!(items.len(), sources.len());

        for (item, source) in items.iter().zip(&sources) {
            let value = serde_json::to_value(item).unwrap();
            let obj = value.as_object().unwrap();
            let keys: Vec<_> = obj.keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                vec![
                    "available_at",
                    "channel_id",
                    "channel_name",
                    "channel_photo",
                    "id",
                    "is_placeholder",
                    "link",
                    "live_viewers",
                    "placeholder_type",
                    "published_at",
                    "start_actual",
                    "start_scheduled",
                    "status",
                    "suggested_area_id",
                    "suggested_area_name",
                    "thumbnail",
                    "title",
                    "topic",
                ]
            );
            for key in [
                "external_link",
                "topic_id",
                "stream_type",
                "command_platform",
                "command_channel",
                "command_channel_short",
                "switchable",
                "reason",
                "reason_keywords",
            ] {
                assert!(!obj.contains_key(key), "still serializes {key}");
            }
            assert_eq!(obj.get("link"), Some(&serde_json::json!(source.link)));
            assert_eq!(obj.get("topic"), Some(&serde_json::json!(source.topic_id)));
            assert_eq!(
                obj.get("is_placeholder").and_then(|v| v.as_bool()),
                Some(source.stream_type == "placeholder")
            );
            assert_eq!(
                obj.get("channel_id").and_then(|flag| flag.as_str()),
                Some(source.channel.id.as_str())
            );
        }
    }
}
