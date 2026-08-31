//! Server-side thumbnail cache.
//!
//! Holodex thumbnail urls point at YouTube and Twitch CDNs, which viewers in
//! some regions cannot reach. The serving node fetches them once and hands out
//! its own copies instead.
//!
//! The current Holodex response is the whole lifecycle: whatever it references
//! is fetched, whatever it stopped referencing is deleted. That bounds the
//! cache by construction — one file per currently listed stream — with no TTL
//! or size sweep to tune.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hosts a thumbnail may be fetched from. An url outside this list is left
/// pointing upstream rather than proxied, so the node cannot be aimed at
/// arbitrary addresses by whatever Holodex returns.
const ALLOWED_HOSTS: &[&str] = &[
    "i.ytimg.com",
    "img.youtube.com",
    "yt3.ggpht.com",
    "yt3.googleusercontent.com",
    "static-cdn.jtvnw.net",
    "vod-secure.twitch.tv",
    "nicovideo.cdn.nimg.jp",
    "img.cdn.nimg.jp",
    "holodex.net",
];

/// A thumbnail past this size is not a thumbnail.
const MAX_BYTES: usize = 512 * 1024;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// A url must be missing from this many consecutive good responses before its
/// file is removed, so a stream flickering out of the list does not cause a
/// delete-then-refetch on every cycle.
const MISSING_ROUNDS_BEFORE_DELETE: u32 = 2;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct CacheEntry {
    content_type: String,
    /// Consecutive good responses that did not mention this url.
    #[serde(default)]
    missing_rounds: u32,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct CacheIndex {
    entries: HashMap<String, CacheEntry>,
}

static INDEX: RwLock<Option<CacheIndex>> = RwLock::new(None);

/// Content-addressed by url, so a changed thumbnail is a new key and the
/// stored bytes can be served as immutable.
pub(super) fn thumbnail_key(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    hex_prefix(&digest, 16)
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .take(chars)
        .map(|nibble| char::from_digit(nibble as u32, 16).unwrap_or('0'))
        .collect()
}

fn cache_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("cache").join("thumbnails"))
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn file_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.img"))
}

/// True when the url is one this node will fetch.
pub(super) fn host_is_allowed(url: &str) -> bool {
    let Some(host) = host_of(url) else {
        return false;
    };
    ALLOWED_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

fn host_of(url: &str) -> Option<String> {
    // Only https is proxied: the node would otherwise fetch cleartext on a
    // viewer's behalf.
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = match host.strip_prefix('[') {
        // IPv6 literal.
        Some(v6) => v6.split(']').next()?.to_string(),
        None => host.split(':').next()?.to_string(),
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// The path a stream's thumbnail should point at, once cached.
pub(super) fn public_path(key: &str) -> String {
    format!("/t/{key}")
}

// Index ---------------------------------------------------------------------

async fn load_index(dir: &Path) -> CacheIndex {
    match tokio::fs::read_to_string(index_path(dir)).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => CacheIndex::default(),
    }
}

async fn save_index(dir: &Path, index: &CacheIndex) {
    let Ok(body) = serde_json::to_string(index) else {
        return;
    };
    if let Err(e) = tokio::fs::write(index_path(dir), body).await {
        tracing::debug!("缩略图索引写入失败: {}", e);
    }
}

fn cached_entry(key: &str) -> Option<CacheEntry> {
    INDEX.read().ok()?.as_ref()?.entries.get(key).cloned()
}

/// The stored bytes and content type for a key.
pub(super) async fn read_thumbnail(key: &str) -> Option<(Vec<u8>, String)> {
    // Reject anything that is not a plain hex key before touching the path,
    // so a crafted key cannot escape the cache directory.
    if !key.chars().all(|c| c.is_ascii_hexdigit()) || key.is_empty() {
        return None;
    }
    let entry = cached_entry(key)?;
    let dir = cache_dir()?;
    let bytes = tokio::fs::read(file_path(&dir, key)).await.ok()?;
    Some((bytes, entry.content_type))
}

// Reconcile -----------------------------------------------------------------

/// Fetches what the current response references and deletes what it no longer
/// does.
///
/// `urls` must come from a *successful* Holodex fetch. An empty list from a
/// failed poll would delete the whole cache and leave the page on placeholders
/// until the next success, so callers skip this entirely on failure.
pub(super) async fn reconcile(urls: &[String]) {
    let Some(dir) = cache_dir() else {
        return;
    };
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return;
    }

    let mut index = match INDEX.read().ok().and_then(|guard| guard.clone()) {
        Some(index) => index,
        None => reconcile_index_with_disk(&dir).await,
    };

    let wanted: Vec<(String, String)> = urls
        .iter()
        .filter(|url| host_is_allowed(url))
        .map(|url| (thumbnail_key(url), url.clone()))
        .collect();

    for (key, url) in &wanted {
        if let Some(entry) = index.entries.get_mut(key) {
            entry.missing_rounds = 0;
            continue;
        }
        if let Some(entry) = fetch_thumbnail(&dir, key, url).await {
            index.entries.insert(key.clone(), entry);
        }
    }

    delete_unreferenced(&dir, &mut index, &wanted).await;
    save_index(&dir, &index).await;
    store_index(index);
}

fn store_index(index: CacheIndex) {
    if let Ok(mut guard) = INDEX.write() {
        *guard = Some(index);
    }
}

/// On the first pass, drop index entries whose file vanished and delete files
/// the index does not know about, so a crash mid-write cannot orphan either.
async fn reconcile_index_with_disk(dir: &Path) -> CacheIndex {
    let mut index = load_index(dir).await;

    index.entries.retain(|key, _| file_path(dir, key).exists());

    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "index.json") {
                continue;
            }
            let known = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| index.entries.contains_key(stem));
            if !known {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }

    index
}

/// Ages entries the response no longer mentions and names the ones that have
/// been absent long enough to remove. Pure so the lifecycle can be tested
/// without a filesystem or a network.
fn mark_missing_and_collect_doomed(index: &mut CacheIndex, wanted_keys: &[String]) -> Vec<String> {
    let mut doomed = Vec::new();

    for (key, entry) in index.entries.iter_mut() {
        if wanted_keys.iter().any(|wanted| wanted == key) {
            continue;
        }
        entry.missing_rounds += 1;
        if entry.missing_rounds >= MISSING_ROUNDS_BEFORE_DELETE {
            doomed.push(key.clone());
        }
    }

    doomed
}

async fn delete_unreferenced(dir: &Path, index: &mut CacheIndex, wanted: &[(String, String)]) {
    let wanted_keys: Vec<String> = wanted.iter().map(|(key, _)| key.clone()).collect();

    for key in mark_missing_and_collect_doomed(index, &wanted_keys) {
        let _ = tokio::fs::remove_file(file_path(dir, &key)).await;
        index.entries.remove(&key);
    }
}

async fn fetch_thumbnail(dir: &Path, key: &str, url: &str) -> Option<CacheEntry> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !content_type.starts_with("image/") {
        return None;
    }

    // Refuse before buffering when the server declares an oversized body.
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_BYTES)
    {
        return None;
    }

    let bytes = response.bytes().await.ok()?;
    if bytes.len() > MAX_BYTES || bytes.is_empty() {
        return None;
    }

    tokio::fs::write(file_path(dir, key), &bytes).await.ok()?;

    Some(CacheEntry {
        content_type,
        missing_rounds: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_url_specific() {
        let a = thumbnail_key("https://i.ytimg.com/vi/abc/maxres.jpg");
        let b = thumbnail_key("https://i.ytimg.com/vi/abc/maxres.jpg");
        let c = thumbnail_key("https://i.ytimg.com/vi/xyz/maxres.jpg");

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn only_known_cdns_are_fetched() {
        assert!(host_is_allowed("https://i.ytimg.com/vi/abc/maxres.jpg"));
        assert!(host_is_allowed(
            "https://static-cdn.jtvnw.net/previews/x.jpg"
        ));
        assert!(host_is_allowed("https://yt3.ggpht.com/a/photo.jpg"));

        assert!(!host_is_allowed("https://evil.example.com/x.jpg"));
        assert!(!host_is_allowed("https://127.0.0.1/x.jpg"));
        assert!(!host_is_allowed("https://[::1]:8080/x.jpg"));
    }

    /// The node fetches these itself, so cleartext would be fetched on a
    /// viewer's behalf over the node's network.
    #[test]
    fn plain_http_is_never_fetched() {
        assert!(!host_is_allowed("http://i.ytimg.com/vi/abc/maxres.jpg"));
        assert!(!host_is_allowed("//i.ytimg.com/vi/abc/maxres.jpg"));
        assert!(!host_is_allowed("i.ytimg.com/vi/abc/maxres.jpg"));
    }

    /// A hostname merely *containing* an allowed one must not pass.
    #[test]
    fn lookalike_hosts_are_refused() {
        assert!(!host_is_allowed("https://i.ytimg.com.evil.example/x.jpg"));
        assert!(!host_is_allowed("https://noti.ytimg.com/x.jpg"));
        assert!(!host_is_allowed("https://evil.example/?h=i.ytimg.com"));
    }

    /// A subdomain of an allowed host is still that CDN.
    #[test]
    fn subdomains_of_allowed_hosts_pass() {
        assert!(host_is_allowed("https://lh3.yt3.ggpht.com/x.jpg"));
    }

    /// Credentials in the authority must not be read as the host.
    #[test]
    fn userinfo_cannot_disguise_the_host() {
        assert!(!host_is_allowed("https://i.ytimg.com@evil.example/x.jpg"));
        assert!(host_is_allowed("https://user@i.ytimg.com/x.jpg"));
    }

    #[test]
    fn ports_do_not_change_the_host() {
        assert!(host_is_allowed("https://i.ytimg.com:443/vi/abc/maxres.jpg"));
    }

    #[tokio::test]
    async fn a_key_that_is_not_hex_is_refused_before_touching_the_path() {
        // Path traversal, or anything that is not a key we minted.
        assert!(read_thumbnail("../../etc/passwd").await.is_none());
        assert!(read_thumbnail("..").await.is_none());
        assert!(read_thumbnail("").await.is_none());
        assert!(read_thumbnail("abc/def").await.is_none());
    }

    fn index_with(keys: &[&str]) -> CacheIndex {
        let mut index = CacheIndex::default();
        for key in keys {
            index.entries.insert(
                key.to_string(),
                CacheEntry {
                    content_type: "image/jpeg".to_string(),
                    missing_rounds: 0,
                },
            );
        }
        index
    }

    fn keys(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    /// A stream that drops out of one response and comes back must not cause a
    /// delete-then-refetch every cycle.
    #[test]
    fn a_flicker_does_not_delete_the_file() {
        let mut index = index_with(&["aaa", "bbb"]);

        // Round one: "bbb" is missing, but not for long enough.
        assert!(mark_missing_and_collect_doomed(&mut index, &keys(&["aaa"])).is_empty());
        assert_eq!(index.entries["bbb"].missing_rounds, 1);

        // It comes back, and the counter resets the way reconcile() does.
        index.entries.get_mut("bbb").unwrap().missing_rounds = 0;
        assert!(mark_missing_and_collect_doomed(&mut index, &keys(&["aaa", "bbb"])).is_empty());
        assert_eq!(index.entries["bbb"].missing_rounds, 0);
    }

    #[test]
    fn an_entry_absent_for_two_rounds_is_deleted() {
        let mut index = index_with(&["aaa", "bbb"]);

        assert!(mark_missing_and_collect_doomed(&mut index, &keys(&["aaa"])).is_empty());
        assert_eq!(
            mark_missing_and_collect_doomed(&mut index, &keys(&["aaa"])),
            keys(&["bbb"])
        );
    }

    #[test]
    fn a_referenced_entry_is_never_doomed() {
        let mut index = index_with(&["aaa"]);

        for _ in 0..5 {
            assert!(mark_missing_and_collect_doomed(&mut index, &keys(&["aaa"])).is_empty());
        }
    }

    /// A crash between writing a file and writing the index must not leave
    /// either side orphaned.
    #[tokio::test]
    async fn startup_reconciles_the_index_against_the_files() {
        let dir = std::env::temp_dir().join(format!(
            "bilistream-thumb-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // "aaa" has both file and index entry, "bbb" only an index entry,
        // "ccc" only a file.
        let mut index = index_with(&["aaa", "bbb"]);
        index.entries.remove("nothing");
        tokio::fs::write(file_path(&dir, "aaa"), b"x")
            .await
            .unwrap();
        tokio::fs::write(file_path(&dir, "ccc"), b"x")
            .await
            .unwrap();
        save_index(&dir, &index).await;

        let reconciled = reconcile_index_with_disk(&dir).await;

        assert!(reconciled.entries.contains_key("aaa"));
        assert!(!reconciled.entries.contains_key("bbb"));
        assert!(file_path(&dir, "aaa").exists());
        assert!(!file_path(&dir, "ccc").exists());
        assert!(index_path(&dir).exists());

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
