//! Public stream image fallbacks, CDN recovery, and cache rewriting.
use super::super::thumbnails;
use super::payload::PublicStream;
use crate::plugins::stream_item::StreamItem;
use crate::plugins::twitch::channel_id_from_url;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Keep the existing failed channels.list retry interval.
const AVATAR_RETRY: Duration = Duration::from_secs(4 * 60);

/// Holodex omits the thumbnail for plain YouTube streams and for Twitch
/// placeholders, so derive what the dashboard derives. Done here rather than
/// in the page so the cache fetches these too.
pub(super) fn thumbnail_for(stream: &StreamItem) -> Option<String> {
    if let Some(thumbnail) = stream.thumbnail.as_deref().filter(|t| !t.is_empty()) {
        return Some(thumbnail.to_string());
    }

    if stream.stream_type == "placeholder" {
        return stream
            .link
            .as_deref()
            .and_then(channel_id_from_url)
            .map(|login| {
                format!("https://static-cdn.jtvnw.net/previews-ttv/live_user_{login}-640x360.jpg")
            });
    }

    (!stream.id.is_empty()).then(|| format!("https://i.ytimg.com/vi/{}/sddefault.jpg", stream.id))
}

/// Holodex serves a channel image for every YouTube channel id. Discovery items
/// often have no `channel.photo`, and the page only paints an avatar from that
/// field. When this host is down, `use_youtube_cdn_avatars` substitutes the
/// `yt3.ggpht.com` url Holodex stores as `channel.photo`.
pub(super) fn channel_avatar_url(channel_id: &str) -> Option<String> {
    // YouTube channel ids are `UC` plus 22 url-safe characters. Anything else
    // must not be interpolated into a fetch URL.
    let youtube_id = channel_id.len() == 24
        && channel_id.starts_with("UC")
        && channel_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    youtube_id.then(|| format!("https://holodex.net/statics/channelImg/{channel_id}/100.png"))
}

fn channel_id_from_holodex_image(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://holodex.net/statics/channelImg/")?;
    let (id, file) = rest.split_once('/')?;
    (file == "100.png" && channel_avatar_url(id).is_some()).then_some(id)
}

/// YouTube CDN avatars learned after a Holodex channel-image miss.
struct CdnAvatars {
    urls: HashMap<String, String>,
    /// Channels YouTube answered with no usable thumbnail.
    missing: HashSet<String>,
    /// A failed `channels.list`. Don't spend another unit until this passes.
    retry_at: Option<Instant>,
}

static CDN_AVATARS: Mutex<Option<CdnAvatars>> = Mutex::new(None);

fn with_cdn_avatars<T>(f: impl FnOnce(&mut CdnAvatars) -> T) -> T {
    let mut guard = CDN_AVATARS.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned YouTube channel-avatar cache");
        poisoned.into_inner()
    });
    f(guard.get_or_insert_with(|| CdnAvatars {
        urls: HashMap::new(),
        missing: HashSet::new(),
        retry_at: None,
    }))
}

/// Replaces a Holodex channel image the node could not fetch with the YouTube
/// CDN url for that channel. A photo Holodex already supplied is left as-is.
fn apply_cdn_avatars(
    streams: &mut [PublicStream],
    cdn_by_channel: &HashMap<String, String>,
    holodex_fetched: impl Fn(&str) -> bool,
) {
    for stream in streams {
        let Some(photo) = stream.channel_photo.clone() else {
            continue;
        };
        let Some(channel_id) = channel_id_from_holodex_image(&photo) else {
            continue;
        };
        if holodex_fetched(&photo) {
            continue;
        }
        let Some(cdn) = cdn_by_channel
            .get(channel_id)
            .filter(|url| thumbnails::host_is_allowed(url))
        else {
            continue;
        };
        stream.channel_photo = Some(cdn.clone());
    }
}

fn uncached_holodex_channel_ids(streams: &[PublicStream]) -> Vec<String> {
    let mut ids = Vec::new();
    for stream in streams {
        let Some(photo) = stream.channel_photo.as_deref() else {
            continue;
        };
        let Some(channel_id) = channel_id_from_holodex_image(photo) else {
            continue;
        };
        if thumbnails::thumbnail_cached(photo) || ids.iter().any(|id| id == channel_id) {
            continue;
        }
        ids.push(channel_id.to_string());
    }
    ids
}

/// When the Holodex channel image is not in the cache, fill `channel.photo`
/// from YouTube's CDN. One `channels.list` unit covers up to 50 channels, and
/// a hit is remembered so the next rebuild does not ask again.
async fn use_youtube_cdn_avatars(streams: &mut [PublicStream], cfg: &crate::config::Config) {
    if crate::cluster::yt_index_role().is_peer() {
        return;
    }
    let now = Instant::now();
    let mut ask = Vec::new();
    let known = with_cdn_avatars(|cache| {
        if cache.retry_at.is_some_and(|at| at <= now) {
            cache.retry_at = None;
        }
        let waiting = cache.retry_at.is_some();
        for id in uncached_holodex_channel_ids(streams) {
            if waiting || cache.urls.contains_key(&id) || cache.missing.contains(&id) {
                continue;
            }
            ask.push(id);
        }
        cache.urls.clone()
    });
    apply_cdn_avatars(streams, &known, thumbnails::thumbnail_cached);
    if ask.is_empty() {
        return;
    }
    let keys = cfg.youtube_api_keys();
    if keys.is_empty() {
        return;
    }
    match crate::plugins::youtube_data::fetch_channel_thumbnails(
        &keys,
        cfg.youtube.proxy.as_deref(),
        &ask,
    )
    .await
    {
        Ok(found) => {
            let accepted = with_cdn_avatars(|cache| {
                for id in &ask {
                    match found.get(id).filter(|url| thumbnails::host_is_allowed(url)) {
                        Some(url) => {
                            cache.urls.insert(id.clone(), url.clone());
                        }
                        None => {
                            cache.missing.insert(id.clone());
                        }
                    }
                }
                cache.urls.clone()
            });
            apply_cdn_avatars(streams, &accepted, thumbnails::thumbnail_cached);
        }
        Err(error) => {
            let first = with_cdn_avatars(|cache| {
                let first = cache.retry_at.is_none();
                cache.retry_at = Some(now + AVATAR_RETRY);
                first
            });
            if first {
                tracing::warn!("YouTube 频道头像查询失败: {}", error);
            } else {
                tracing::debug!("YouTube 频道头像查询失败: {}", error);
            }
        }
    }
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
    if thumbnails::host_is_allowed(src) {
        *url = Some(thumbnails::public_path(&thumbnails::thumbnail_key(src)));
    } else {
        *url = None;
    }
}

pub(super) async fn prepare_images(streams: &mut [PublicStream], cfg: &crate::config::Config) {
    // Channel photos use the same cache: the page's CSP only allows same-origin
    // images, and those CDNs are often unreachable from the viewer's network.
    // A Holodex channel image that did not cache is replaced with the YouTube
    // CDN url before the page is served.
    if thumbnails::holodex_channel_images_down() {
        let known = with_cdn_avatars(|cache| cache.urls.clone());
        apply_cdn_avatars(streams, &known, thumbnails::thumbnail_cached);
    }
    let fetched = image_urls(streams);
    thumbnails::reconcile(&fetched).await;
    use_youtube_cdn_avatars(streams, cfg).await;
    let resolved = image_urls(streams);
    if resolved != fetched {
        thumbnails::reconcile(&resolved).await;
    }
    rewrite_thumbnails(streams);
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{build, stream};
    use super::*;
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
    fn a_channel_without_a_photo_uses_the_holodex_channel_image() {
        let yt = stream("ねむ", None, "UCX4WL24YEOUYd7qDsFSLDOw");
        let built = build(vec![yt], true, &[]);
        let photo = "https://holodex.net/statics/channelImg/UCX4WL24YEOUYd7qDsFSLDOw/100.png";
        assert_eq!(built[0].channel_photo.as_deref(), Some(photo));
        assert!(image_urls(&built).iter().any(|url| url == photo));

        let mut url = built[0].channel_photo.clone();
        rewrite_image(&mut url);
        assert!(url.unwrap().starts_with("/t/"));

        let mut kept = stream("ランク", None, "UCX4WL24YEOUYd7qDsFSLDOw");
        kept.channel.photo = Some("https://yt3.ggpht.com/a.jpg".to_string());
        let built = build(vec![kept], true, &[]);
        assert_eq!(
            built[0].channel_photo.as_deref(),
            Some("https://yt3.ggpht.com/a.jpg")
        );

        let odd = stream("x", None, "not-a-channel");
        assert_eq!(build(vec![odd], true, &[])[0].channel_photo, None);
    }

    #[test]
    fn a_failed_holodex_channel_image_uses_the_youtube_cdn() {
        let channel = "UCX4WL24YEOUYd7qDsFSLDOw";
        let cdn = "https://yt3.ggpht.com/a=s800-c-k-c0x00ffffff-no-rj";
        let mut found = HashMap::new();
        found.insert(channel.to_string(), cdn.to_string());

        let mut missed = build(vec![stream("ねむ", None, channel)], true, &[]);
        apply_cdn_avatars(&mut missed, &found, |_| false);
        assert_eq!(missed[0].channel_photo.as_deref(), Some(cdn));

        let mut fetched = build(vec![stream("ねむ", None, channel)], true, &[]);
        apply_cdn_avatars(&mut fetched, &found, |_| true);
        assert_eq!(
            fetched[0].channel_photo.as_deref(),
            Some("https://holodex.net/statics/channelImg/UCX4WL24YEOUYd7qDsFSLDOw/100.png")
        );

        found.insert(
            channel.to_string(),
            "https://evil.example/a.jpg".to_string(),
        );
        let mut rejected = build(vec![stream("ねむ", None, channel)], true, &[]);
        apply_cdn_avatars(&mut rejected, &found, |_| false);
        assert_eq!(
            rejected[0].channel_photo.as_deref(),
            Some("https://holodex.net/statics/channelImg/UCX4WL24YEOUYd7qDsFSLDOw/100.png")
        );

        let mut kept = stream("ランク", None, channel);
        kept.channel.photo = Some("https://yt3.ggpht.com/already.jpg".to_string());
        let mut kept = build(vec![kept], true, &[]);
        apply_cdn_avatars(&mut kept, &found, |_| false);
        assert_eq!(
            kept[0].channel_photo.as_deref(),
            Some("https://yt3.ggpht.com/already.jpg")
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
}
