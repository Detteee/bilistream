//! One Holodex channels-mode response, shared by the dashboard and the public
//! page.
//!
//! On the node that serves both, an operator opening the dashboard issues the
//! same `users/live?channels=…` query the public timer does. Caching the raw
//! response lets whichever ran first satisfy the other, so the two together
//! still cost one upstream call per interval.
//!
//! Keyed on the requested channel set: the dashboard builds its id list
//! separately, and a list that does not match must never be reused. Favorites
//! mode is a different query over a different channel set and never lands here.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::plugins::holodex::HolodexStream;

struct Entry {
    ids_key: String,
    streams: Vec<HolodexStream>,
    fetched_at: Instant,
}

static CACHE: RwLock<Option<Entry>> = RwLock::new(None);

/// Order-independent key for a channel set, so the two callers building the
/// same set in a different order still share a hit.
fn ids_key(ids: &[String]) -> String {
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(",")
}

/// The cached response for this channel set, if it is younger than `max_age`.
pub fn get_if_fresh(ids: &[String], max_age: Duration) -> Option<Vec<HolodexStream>> {
    let key = ids_key(ids);
    let guard = CACHE.read().ok()?;
    let entry = guard.as_ref()?;

    if entry.ids_key != key || entry.fetched_at.elapsed() >= max_age {
        return None;
    }
    Some(entry.streams.clone())
}

pub fn put(ids: &[String], streams: &[HolodexStream]) {
    if let Ok(mut guard) = CACHE.write() {
        *guard = Some(Entry {
            ids_key: ids_key(ids),
            streams: streams.to_vec(),
            fetched_at: Instant::now(),
        });
    }
}

/// Cached response if fresh, otherwise one upstream call whose result is
/// cached for the other caller.
pub async fn get_or_fetch(
    ids: Vec<String>,
    max_age: Duration,
) -> Result<Vec<HolodexStream>, Box<dyn std::error::Error>> {
    if let Some(cached) = get_if_fresh(&ids, max_age) {
        return Ok(cached);
    }

    let streams = crate::plugins::holodex::get_holodex_streams(ids.clone(), true).await?;
    put(&ids, &streams);
    Ok(streams)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::holodex::HolodexChannel;
    use std::sync::{Mutex, MutexGuard};

    /// The cache is process-wide, so these tests cannot run concurrently.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn exclusive() -> MutexGuard<'static, ()> {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Ok(mut cache) = CACHE.write() {
            *cache = None;
        }
        guard
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn stream(id: &str) -> HolodexStream {
        HolodexStream {
            id: id.to_string(),
            title: "t".to_string(),
            stream_type: "stream".to_string(),
            topic_id: None,
            published_at: None,
            available_at: None,
            status: "live".to_string(),
            start_scheduled: None,
            start_actual: None,
            live_viewers: None,
            channel: HolodexChannel::default(),
            link: None,
            thumbnail: None,
            placeholder_type: None,
        }
    }

    #[test]
    fn a_fresh_entry_is_reused() {
        let _guard = exclusive();
        put(&ids(&["a", "b"]), &[stream("vid")]);

        let hit = get_if_fresh(&ids(&["a", "b"]), Duration::from_secs(60)).expect("cache hit");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].id, "vid");
    }

    #[test]
    fn the_channel_set_order_does_not_matter() {
        let _guard = exclusive();
        put(&ids(&["b", "a"]), &[stream("vid")]);

        assert!(get_if_fresh(&ids(&["a", "b"]), Duration::from_secs(60)).is_some());
    }

    /// The dashboard may query a different set; reusing it would show the
    /// public page channels it is not supposed to list.
    #[test]
    fn a_different_channel_set_is_never_reused() {
        let _guard = exclusive();
        put(&ids(&["a", "b"]), &[stream("vid")]);

        assert!(get_if_fresh(&ids(&["a"]), Duration::from_secs(60)).is_none());
        assert!(get_if_fresh(&ids(&["a", "b", "c"]), Duration::from_secs(60)).is_none());
    }

    #[test]
    fn an_entry_past_max_age_is_not_reused() {
        let _guard = exclusive();
        put(&ids(&["a"]), &[stream("vid")]);

        assert!(get_if_fresh(&ids(&["a"]), Duration::from_secs(0)).is_none());
    }

    #[test]
    fn an_empty_cache_is_a_miss() {
        let _guard = exclusive();
        assert!(get_if_fresh(&ids(&["a"]), Duration::from_secs(60)).is_none());
    }
}
