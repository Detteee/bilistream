//! Internal display facts shared by list consumers, never an HTTP payload.
use crate::plugins::stream_item::StreamItem;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamListFacts {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) topic: Option<String>,
    pub(crate) status: String,
    pub(crate) start_scheduled: Option<String>,
    pub(crate) start_actual: Option<String>,
    pub(crate) available_at: Option<String>,
    pub(crate) published_at: Option<String>,
    pub(crate) live_viewers: Option<i32>,
    pub(crate) channel_id: String,
    pub(crate) channel_name: String,
    pub(crate) channel_photo: Option<String>,
    pub(crate) suggested_area_id: Option<u64>,
    pub(crate) suggested_area_name: Option<String>,
    pub(crate) is_placeholder: bool,
    pub(crate) placeholder_type: Option<String>,
    pub(crate) link: Option<String>,
    pub(crate) thumbnail: Option<String>,
}

/// Resolve at each projection point so current area edits are visible even
/// when the source snapshot has not rebuilt. Keep original links and images.
pub(crate) fn resolve_stream_facts(stream: &StreamItem) -> StreamListFacts {
    let (suggested_area_id, suggested_area_name) =
        crate::plugins::suggest_area_from_stream(stream.topic_id.as_deref(), &stream.title);
    StreamListFacts {
        id: stream.id.clone(),
        title: stream.title.clone(),
        topic: stream.topic_id.clone(),
        status: stream.status.clone(),
        start_scheduled: stream.start_scheduled.clone(),
        start_actual: stream.start_actual.clone(),
        available_at: stream.available_at.clone(),
        published_at: stream.published_at.clone(),
        live_viewers: stream.live_viewers,
        channel_id: stream.channel.id.clone(),
        channel_name: stream.channel.name.clone(),
        channel_photo: stream
            .channel
            .photo
            .clone()
            .filter(|photo| !photo.is_empty()),
        suggested_area_id,
        suggested_area_name,
        is_placeholder: stream.stream_type == "placeholder",
        placeholder_type: stream.placeholder_type.clone(),
        link: stream.link.clone(),
        thumbnail: stream.thumbnail.clone(),
    }
}

/// Discovery order is unstable. Stable sorting ignores distinct-ID reorder
/// while retaining duplicate items and their existing relative order.
pub(super) fn list_content(streams: &[StreamItem]) -> Vec<StreamListFacts> {
    let mut facts: Vec<_> = streams.iter().map(resolve_stream_facts).collect();
    facts.sort_by(|a, b| a.id.cmp(&b.id));
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> StreamItem {
        serde_json::from_value(serde_json::json!({
            "id": "a", "title": "Example", "type": "stream", "status": "live",
            "channel": { "id": "UCexample", "name": "Example" }
        }))
        .unwrap()
    }

    #[test]
    fn content_comparison_preserves_mapped_facts_and_duplicate_rows() {
        let item = item();
        let original = list_content(std::slice::from_ref(&item));
        let mut internal = item.clone();
        internal.yt_confirmed = true;
        internal.stream_type = "other".to_string();
        internal.channel.photo = Some(String::new());
        assert_eq!(original, list_content(&[internal]));

        let mut visible = item.clone();
        visible.live_viewers = Some(1);
        assert_ne!(original, list_content(&[visible]));
        let mut visible = item.clone();
        visible.title = "Changed title".to_string();
        assert_ne!(original, list_content(&[visible]));
        let mut other = item.clone();
        other.id = "b".to_string();
        assert_eq!(
            list_content(&[item.clone(), other.clone()]),
            list_content(&[other, item.clone()])
        );
        assert_ne!(original, list_content(&[item.clone(), item.clone()]));

        let mut duplicate = item.clone();
        duplicate.link = Some("https://www.twitch.tv/example_channel_001".to_string());
        let duplicates = list_content(&[item.clone(), duplicate.clone()]);
        assert_eq!(duplicates.len(), 2);
        assert_eq!(duplicates[0].link, None);
        assert_eq!(duplicates[1].link, duplicate.link);
        assert_ne!(duplicates, list_content(&[duplicate, item]));
    }

    #[test]
    fn facts_keep_source_values_and_normalize_only_the_documented_fields() {
        let mut item = item();
        item.topic_id = Some("Example topic".to_string());
        item.stream_type = "placeholder".to_string();
        item.channel.photo = Some(String::new());
        item.link = Some(" https://www.twitch.tv/Example_Channel_001?x=1 ".to_string());
        item.thumbnail = Some(String::new());
        let facts = resolve_stream_facts(&item);
        assert_eq!(facts.topic, item.topic_id);
        assert!(facts.is_placeholder);
        assert_eq!(facts.channel_photo, None);
        assert_eq!(facts.link, item.link);
        assert_eq!(facts.thumbnail, Some(String::new()));
    }
}
