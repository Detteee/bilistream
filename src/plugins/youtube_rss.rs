//! Roster discovery from YouTube's channel Atom feeds.
//!
//! Holodex sometimes never returns a stream at all, and without a video ID the
//! `videos.list` overlay has nothing to correct. The public
//! `feeds/videos.xml?channel_id=` feed lists each channel's latest uploads,
//! waiting rooms included, at no quota cost. A background worker reads the
//! roster's feeds, has `videos.list` classify the IDs, and keeps the ones that
//! are live or upcoming so they can be merged into Holodex results.
//!
//! Atom carries no live status, so the worker only runs while a YouTube Data
//! API key is configured.

use super::holodex::{HolodexChannel, HolodexStream};
use super::http::{pooled_client, response_bytes_limited};
use crate::config::{load_config, Config};
use futures_util::stream::{self, StreamExt};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const FEED_URL: &str = "https://www.youtube.com/feeds/videos.xml?channel_id=";
const TICK: Duration = Duration::from_secs(600);
const FEED_CONCURRENCY: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct FeedEntry {
    pub video_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    pub published: Option<String>,
}

fn entry_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<entry>(.*?)</entry>").expect("valid regex"))
}

fn tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)<yt:videoId>(?P<vid>[^<]*)</yt:videoId>|<yt:channelId>(?P<cid>[^<]*)</yt:channelId>|<name>(?P<name>[^<]*)</name>|<published>(?P<pub>[^<]*)</published>|<title>(?P<title>[^<]*)</title>",
        )
        .expect("valid regex")
    })
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Entries of one channel feed. Only the first of each tag inside an entry
/// counts, so `<media:title>` and nested descriptions cannot override it.
pub fn parse_feed(xml: &str) -> Vec<FeedEntry> {
    entry_regex()
        .captures_iter(xml)
        .filter_map(|entry| {
            let body = entry.get(1)?.as_str();
            let mut parsed = FeedEntry {
                video_id: String::new(),
                channel_id: String::new(),
                channel_name: String::new(),
                title: String::new(),
                published: None,
            };
            for tag in tag_regex().captures_iter(body) {
                let set = |slot: &mut String, name: &str| {
                    if slot.is_empty() {
                        if let Some(value) = tag.name(name) {
                            *slot = unescape(value.as_str().trim());
                        }
                    }
                };
                set(&mut parsed.video_id, "vid");
                set(&mut parsed.channel_id, "cid");
                set(&mut parsed.channel_name, "name");
                set(&mut parsed.title, "title");
                if parsed.published.is_none() {
                    parsed.published = tag.name("pub").map(|m| m.as_str().trim().to_string());
                }
            }
            (!parsed.video_id.is_empty() && !parsed.channel_id.is_empty()).then_some(parsed)
        })
        .collect()
}

/// YouTube channel IDs from channels.json plus the active channel.
async fn roster_channel_ids(cfg: &Config) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |id: &str| {
        let id = id.trim();
        if !id.is_empty() && seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
    };

    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("channels.json")));
    if let Some(path) = path {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(channels) = json.get("channels").and_then(|v| v.as_array()) {
                    for channel in channels {
                        if let Some(id) = channel
                            .get("platforms")
                            .and_then(|p| p.get("youtube"))
                            .and_then(|v| v.as_str())
                        {
                            push(id);
                        }
                    }
                } else if let Some(channels) = json.get("YT_channels").and_then(|v| v.as_array()) {
                    for channel in channels {
                        if let Some(id) = channel.get("channel_id").and_then(|v| v.as_str()) {
                            push(id);
                        }
                    }
                }
            }
        }
    }
    push(&cfg.youtube.channel_id);
    ids
}

#[derive(Default)]
struct Discovery {
    /// Live or upcoming rows, already classified by YouTube.
    found: HashMap<String, HolodexStream>,
    /// IDs YouTube called a plain upload, ended, or omitted. Pruned to the IDs
    /// still in some feed, so it stays bounded.
    ignored: HashSet<String>,
}

static DISCOVERY: Mutex<Option<Discovery>> = Mutex::new(None);

fn base_row(entry: &FeedEntry) -> HolodexStream {
    HolodexStream {
        id: entry.video_id.clone(),
        title: entry.title.clone(),
        stream_type: "stream".to_string(),
        topic_id: None,
        published_at: entry.published.clone(),
        available_at: None,
        status: "upcoming".to_string(),
        start_scheduled: None,
        start_actual: None,
        live_viewers: None,
        channel: HolodexChannel {
            id: entry.channel_id.clone(),
            name: entry.channel_name.clone(),
            photo: None,
        },
        link: None,
        thumbnail: None,
        placeholder_type: None,
        yt_confirmed: false,
    }
}

async fn fetch_feed(client: &reqwest::Client, channel_id: &str) -> Result<Vec<FeedEntry>, String> {
    let response = client
        .get(format!("{FEED_URL}{channel_id}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("feed {channel_id}: {}", response.status()));
    }
    let bytes = response_bytes_limited(response, 2 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    Ok(parse_feed(&String::from_utf8_lossy(&bytes)))
}

async fn tick() -> Result<(), String> {
    let cfg = load_config().await.map_err(|e| e.to_string())?;
    let Some(api_key) = cfg.youtube_api_key.clone().filter(|key| !key.is_empty()) else {
        // Nothing can be classified without a key; drop stale rows too.
        *DISCOVERY.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Ok(());
    };
    let roster = roster_channel_ids(&cfg).await;
    let client = pooled_client(cfg.youtube.proxy.as_deref()).map_err(|e| e.to_string())?;

    // Owned items: futures borrowing the roster make tick() unspawnable.
    let feeds: Vec<Result<Vec<FeedEntry>, String>> = stream::iter(roster)
        .map(|id| {
            let client = client.clone();
            async move { fetch_feed(&client, &id).await }
        })
        .buffer_unordered(FEED_CONCURRENCY)
        .collect()
        .await;
    let mut entries: HashMap<String, FeedEntry> = HashMap::new();
    for feed in feeds {
        match feed {
            Ok(feed) => entries.extend(feed.into_iter().map(|e| (e.video_id.clone(), e))),
            Err(e) => tracing::debug!("YouTube RSS 读取失败: {}", e),
        }
    }

    let (candidates, known) = {
        let mut guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
        let state = guard.get_or_insert_with(Discovery::default);
        state.ignored.retain(|id| entries.contains_key(id));
        let known: Vec<HolodexStream> = state.found.values().cloned().collect();
        let mut candidates: Vec<String> = entries
            .keys()
            .filter(|id| !state.found.contains_key(*id) && !state.ignored.contains(*id))
            .cloned()
            .collect();
        candidates.extend(known.iter().map(|row| row.id.clone()));
        (candidates, known)
    };
    if candidates.is_empty() {
        return Ok(());
    }

    let videos =
        super::youtube_data::videos_for(&api_key, cfg.youtube.proxy.as_deref(), candidates.clone())
            .await
            .map_err(|e| e.to_string())?;

    let known_ids: HashSet<&str> = known.iter().map(|row| row.id.as_str()).collect();
    let new_rows: Vec<HolodexStream> = candidates
        .iter()
        .filter(|id| !known_ids.contains(id.as_str()))
        .filter_map(|id| entries.get(id))
        .map(base_row)
        .collect();
    // Rows YouTube omitted would pass through the overlay unchanged; they are
    // private or deleted, so drop them here instead.
    let rows: Vec<HolodexStream> = known
        .into_iter()
        .chain(new_rows)
        .filter(|row| videos.contains_key(&row.id))
        .collect();
    let classified = super::youtube_data::overlay(rows, &videos);

    let mut guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(Discovery::default);
    state.found = classified
        .into_iter()
        .map(|row| (row.id.clone(), row))
        .collect();
    for id in candidates {
        if !state.found.contains_key(&id) {
            state.ignored.insert(id);
        }
    }
    Ok(())
}

/// Holodex rows plus discovered live/upcoming rows Holodex did not send.
/// Callers filter by channel and horizon afterwards as usual.
pub fn merge_discovered(streams: Vec<HolodexStream>) -> Vec<HolodexStream> {
    let guard = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_ref() else {
        return streams;
    };
    merge_rows(streams, state.found.values())
}

fn merge_rows<'a>(
    mut streams: Vec<HolodexStream>,
    found: impl Iterator<Item = &'a HolodexStream>,
) -> Vec<HolodexStream> {
    let present: HashSet<String> = streams.iter().map(|s| s.id.clone()).collect();
    streams.extend(found.filter(|row| !present.contains(&row.id)).cloned());
    streams
}

static RSS_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub struct RssDiscoveryWorker(tokio::task::JoinHandle<()>);

impl Drop for RssDiscoveryWorker {
    fn drop(&mut self) {
        self.0.abort();
        RSS_WORKER_STARTED.store(false, Ordering::SeqCst);
    }
}

pub fn start_rss_discovery_worker() -> Option<RssDiscoveryWorker> {
    if RSS_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(RssDiscoveryWorker(tokio::spawn(async {
        loop {
            if let Err(e) = tick().await {
                tracing::warn!("YouTube RSS 发现失败: {}", e);
            }
            tokio::time::sleep(TICK).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom">
 <yt:channelId>gYCMluaLpERsyNXlPOvBtA</yt:channelId>
 <title>Channel</title>
 <entry>
  <id>yt:video:BnV1AEeqBR4</id>
  <yt:videoId>BnV1AEeqBR4</yt:videoId>
  <yt:channelId>UCgYCMluaLpERsyNXlPOvBtA</yt:channelId>
  <title>Bomb &amp; defuse</title>
  <author><name>かみと</name></author>
  <published>2026-09-24T10:59:38+00:00</published>
  <media:group><media:title>other</media:title></media:group>
 </entry>
 <entry>
  <yt:videoId>second</yt:videoId>
  <yt:channelId>UCgYCMluaLpERsyNXlPOvBtA</yt:channelId>
  <title>Two</title>
 </entry>
</feed>"#;

    #[test]
    fn parses_each_entry_with_its_full_channel_id() {
        let entries = parse_feed(FEED);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            FeedEntry {
                video_id: "BnV1AEeqBR4".to_string(),
                channel_id: "UCgYCMluaLpERsyNXlPOvBtA".to_string(),
                channel_name: "かみと".to_string(),
                title: "Bomb & defuse".to_string(),
                published: Some("2026-09-24T10:59:38+00:00".to_string()),
            }
        );
        assert_eq!(entries[1].video_id, "second");
        assert_eq!(entries[1].published, None);
    }

    #[test]
    fn merging_adds_only_ids_holodex_did_not_send() {
        let entry = |id: &str| FeedEntry {
            video_id: id.to_string(),
            channel_id: "UC1".to_string(),
            channel_name: String::new(),
            title: format!("{id} rss"),
            published: None,
        };
        let mut holodex = base_row(&entry("shared"));
        holodex.title = "holodex".to_string();
        let found = [base_row(&entry("shared")), base_row(&entry("missing"))];

        let merged = merge_rows(vec![holodex], found.iter());
        let ids: Vec<&str> = merged.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["shared", "missing"]);
        assert_eq!(merged[0].title, "holodex");
    }
}
