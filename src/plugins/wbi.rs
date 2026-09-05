use lazy_static::lazy_static;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::utils::md5_hex;

const WBI_CACHE_DURATION: u64 = 12 * 60 * 60; // 12 hours in seconds

const MIXIN_KEY_ENC_TAB: [u8; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

lazy_static! {
    static ref WBI_CACHE_DIR: PathBuf = wbi_cache_dir(&executable_path());
}

fn executable_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bilistream"))
}

fn executable_parent_dir(path: &Path) -> Option<PathBuf> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

fn wbi_cache_dir(executable: &Path) -> PathBuf {
    executable_parent_dir(executable)
        .map(|parent| parent.join(".wbi_cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("bilistream-wbi-cache"))
}

fn unix_time_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn current_unix_time_secs() -> u64 {
    unix_time_secs(SystemTime::now())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn gen_mixin_key(raw_wbi_key: impl AsRef<[u8]>) -> Result<String, io::Error> {
    let raw_wbi_key = raw_wbi_key.as_ref();
    let mut mixin_key = String::with_capacity(32);

    for &index in MIXIN_KEY_ENC_TAB.iter().take(32) {
        let byte = raw_wbi_key.get(index as usize).ok_or_else(|| {
            invalid_data(format!(
                "invalid WBI key length: {} bytes, missing index {}",
                raw_wbi_key.len(),
                index
            ))
        })?;
        mixin_key.push(*byte as char);
    }

    Ok(mixin_key)
}

fn url_encode(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC)
        .to_string()
        .replace('+', "%20")
}

fn calculate_w_rid(params: &BTreeMap<&str, String>, mixin_key: &str) -> String {
    let encoded_params: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, url_encode(v)))
        .collect();
    let param_string = encoded_params.join("&");
    let string_to_hash = format!("{}{}", param_string, mixin_key);
    md5_hex(&string_to_hash)
}

fn wbi_key_from_url(url: &str, field: &str) -> Result<String, io::Error> {
    url.split('/')
        .next_back()
        .and_then(|segment| segment.split('.').next())
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid_data(format!("invalid {field} WBI URL: {url}")))
}

async fn get_wbi_keys(agent: &reqwest::Client) -> Result<(String, String), Box<dyn Error>> {
    fs::create_dir_all(&*WBI_CACHE_DIR)?;

    let img_key_path = WBI_CACHE_DIR.join("img_key");
    let sub_key_path = WBI_CACHE_DIR.join("sub_key");
    let timestamp_path = WBI_CACHE_DIR.join("timestamp");

    if img_key_path.exists() && sub_key_path.exists() && timestamp_path.exists() {
        if let Ok(timestamp_str) = fs::read_to_string(&timestamp_path) {
            if let Ok(timestamp) = timestamp_str.parse::<u64>() {
                let current_time = current_unix_time_secs();

                if current_time >= timestamp && current_time - timestamp < WBI_CACHE_DURATION {
                    if let (Ok(img_key), Ok(sub_key)) = (
                        fs::read_to_string(&img_key_path),
                        fs::read_to_string(&sub_key_path),
                    ) {
                        return Ok((img_key.trim().to_string(), sub_key.trim().to_string()));
                    }
                }
            }
        }
    }

    let nav_data: Value = agent
        .get("https://api.bilibili.com/x/web-interface/nav")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .header("Referer", "https://www.bilibili.com/")
        .send()
        .await?
        .json()
        .await?;

    let wbi_img = nav_data
        .get("data")
        .and_then(|d| d.get("wbi_img"))
        .ok_or("Missing wbi_img in nav response")?;

    let img_url = wbi_img
        .get("img_url")
        .and_then(|v| v.as_str())
        .ok_or("Missing img_url in wbi_img")?;

    let sub_url = wbi_img
        .get("sub_url")
        .and_then(|v| v.as_str())
        .ok_or("Missing sub_url in wbi_img")?;

    let img_key = wbi_key_from_url(img_url, "img_url")?;
    let sub_key = wbi_key_from_url(sub_url, "sub_url")?;

    fs::write(&img_key_path, &img_key)?;
    fs::write(&sub_key_path, &sub_key)?;
    fs::write(&timestamp_path, current_unix_time_secs().to_string())?;

    Ok((img_key, sub_key))
}

/// Sign `params` with WBI (`wts` + `w_rid`) and return the query string.
pub(crate) async fn signed_query(
    agent: &reqwest::Client,
    mut params: BTreeMap<&str, String>,
) -> Result<String, Box<dyn Error>> {
    let (img_key, sub_key) = get_wbi_keys(agent).await?;
    let mixin_key = gen_mixin_key(format!("{}{}", img_key, sub_key))?;
    let wts = current_unix_time_secs().to_string();
    params.insert("wts", wts);
    let w_rid = calculate_w_rid(&params, &mixin_key);

    let mut query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
    query.push(format!("w_rid={w_rid}"));
    Ok(query.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_WBI_KEY: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab";

    #[test]
    fn mixin_key_rejects_short_wbi_key() {
        assert!(gen_mixin_key("short").is_err());
    }

    #[test]
    fn mixin_key_accepts_full_wbi_key() {
        let key = gen_mixin_key(FULL_WBI_KEY).expect("64-byte WBI key should be accepted");
        assert_eq!(key, "UVsc1ixGpYkF6dTJBRfXHjQtDCoNmMPn");
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn w_rid_hashes_sorted_encoded_params() {
        let mixin = gen_mixin_key(FULL_WBI_KEY).unwrap();
        let mut params = BTreeMap::new();
        params.insert("id", "1".to_string());
        params.insert("wts", "0".to_string());
        assert_eq!(
            calculate_w_rid(&params, &mixin),
            "67492f9892448babed772849f0432714"
        );
    }

    #[test]
    fn wbi_key_from_url_extracts_file_stem() {
        assert_eq!(
            wbi_key_from_url("https://i0.hdslb.com/bfs/wbi/example-key.png", "img_url").unwrap(),
            "example-key"
        );
        assert!(wbi_key_from_url("https://i0.hdslb.com/bfs/wbi/", "img_url").is_err());
    }

    #[test]
    fn wbi_cache_dir_falls_back_without_executable_parent() {
        assert_eq!(
            wbi_cache_dir(Path::new("bilistream")),
            std::env::temp_dir().join("bilistream-wbi-cache")
        );
    }
}
