//! One immutable asset snapshot per listener, including its module graph.
use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Router};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

// Keep public shared assets explicit: never expose the whole admin directory.
const PUBLIC_SHARED: &[&str] = &[
    "shared.css",
    "responsive.css",
    "js/dom.js",
    "js/format.js",
    "js/dialog.js",
    "js/cluster-health.js",
    "js/cluster-network.js",
    "js/status-cards.js",
];

struct Asset {
    bytes: Bytes,
    mime: &'static str,
    etag: String,
}

impl Asset {
    fn new(path: &str, bytes: Vec<u8>) -> Self {
        let mime = match path.rsplit('.').next().unwrap_or_default() {
            "html" => "text/html; charset=utf-8",
            "js" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "png" => "image/png",
            "svg" => "image/svg+xml",
            _ => "application/octet-stream",
        };
        Self {
            etag: format!("W/\"{:x}\"", Sha256::digest(&bytes)),
            bytes: bytes.into(),
            mime,
        }
    }

    fn response(&self, request: &Request, immutable: bool) -> Response {
        let headers = [
            (header::CONTENT_TYPE, self.mime.to_string()),
            (header::ETAG, self.etag.clone()),
            (
                header::CACHE_CONTROL,
                if immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                }
                .to_string(),
            ),
        ];
        if not_modified(request.headers(), &self.etag) {
            (StatusCode::NOT_MODIFIED, headers).into_response()
        } else if request.method() == Method::HEAD {
            (
                StatusCode::OK,
                headers,
                [(header::CONTENT_LENGTH, self.bytes.len().to_string())],
            )
                .into_response()
        } else {
            (StatusCode::OK, headers, self.bytes.clone()).into_response()
        }
    }
}

pub(crate) struct StaticAssets {
    files: BTreeMap<String, Asset>,
    version: String,
}

impl StaticAssets {
    pub(crate) fn load(public: bool) -> Self {
        let prefix = if public {
            "webui/public-dist/"
        } else {
            "webui/dist/"
        };
        let directory = super::server::static_asset_dir(prefix.trim_end_matches('/'));
        let shared = super::server::static_asset_dir("webui/dist");
        let mut files = BTreeMap::new();
        for &(path, bundled) in super::assets::BUNDLED_ASSETS {
            let selected = if let Some(relative) = path.strip_prefix(prefix) {
                Some((relative.to_string(), directory.join(relative)))
            } else if public {
                path.strip_prefix("webui/dist/")
                    .filter(|relative| PUBLIC_SHARED.contains(relative))
                    .map(|relative| (format!("shared/{relative}"), shared.join(relative)))
            } else {
                None
            };
            if let Some((name, source)) = selected {
                // Fresh installs can serve the matching embedded assets before
                // background dependency setup has written them to disk.
                let bytes = std::fs::read(source).unwrap_or_else(|_| bundled.to_vec());
                files.insert(name, bytes);
            }
        }
        Self::from_files(files)
    }

    fn from_files(mut files: BTreeMap<String, Vec<u8>>) -> Self {
        let mut hash = Sha256::new();
        for (name, bytes) in &files {
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let version = format!("{:x}", hash.finalize());
        let mut index = String::from_utf8_lossy(&files["index.html"]).into_owned();
        for name in files.keys().filter(|name| name.as_str() != "index.html") {
            for attribute in ["src", "href"] {
                for path in [name.clone(), format!("/{name}")] {
                    index = index.replace(
                        &format!("{attribute}=\"{path}\""),
                        &format!("{attribute}=\"/_assets/{version}/{name}\""),
                    );
                }
            }
        }
        files.insert("index.html".into(), index.into_bytes());
        Self {
            version,
            files: files
                .into_iter()
                .map(|(name, bytes)| {
                    let asset = Asset::new(&name, bytes);
                    (name, asset)
                })
                .collect(),
        }
    }

    fn serve(&self, request: Request, spa: bool) -> Response {
        let path = request.uri().path().trim_start_matches('/');
        let (path, immutable) = if let Some(versioned) = path.strip_prefix("_assets/") {
            let Some((version, path)) = versioned.split_once('/') else {
                return StatusCode::NOT_FOUND.into_response();
            };
            if version != self.version {
                return StatusCode::NOT_FOUND.into_response();
            }
            (path, path != "index.html")
        } else {
            (path, false)
        };
        let path = if path.is_empty() { "index.html" } else { path };
        if let Some(asset) = self.files.get(path) {
            return asset.response(&request, immutable);
        }
        if spa && !immutable && !path.contains('.') && !path.starts_with("_assets/") {
            return self.files["index.html"].response(&request, false);
        }
        StatusCode::NOT_FOUND.into_response()
    }
}

pub(crate) fn router(public: bool) -> Router {
    let assets = Arc::new(StaticAssets::load(public));
    Router::new().fallback(get(move |request: Request| {
        let assets = Arc::clone(&assets);
        async move { assets.serve(request, !public) }
    }))
}

pub(crate) fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    let expected = etag.strip_prefix("W/").unwrap_or(etag);
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|requested| {
            requested.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == expected
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_versions_pin_the_whole_module_graph_and_revalidate_html() {
        let mut files = BTreeMap::from([
            (
                "index.html".into(),
                br#"<link href="styles.css"><script src="js/main.js"></script>"#.to_vec(),
            ),
            ("styles.css".into(), b"body{}".to_vec()),
            ("js/main.js".into(), b"import './child.js';".to_vec()),
            ("js/child.js".into(), b"export const value = 1;".to_vec()),
        ]);
        let first = StaticAssets::from_files(files.clone());
        let html = String::from_utf8_lossy(&first.files["index.html"].bytes);
        assert!(html.contains(&format!("/_assets/{}/js/main.js", first.version)));
        let request = |path: &str| {
            Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let path = format!("/_assets/{}/js/child.js", first.version);
        let asset = first.serve(request(&path), true);
        assert_eq!(
            asset.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let mut conditional = request("/");
        conditional.headers_mut().insert(
            header::IF_NONE_MATCH,
            first.files["index.html"].etag.parse().unwrap(),
        );
        let response = first.serve(conditional, true);
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        files.insert("js/child.js".into(), b"export const value = 2;".to_vec());
        let second = StaticAssets::from_files(files);
        assert_ne!(first.version, second.version);
        assert_ne!(
            first.files["index.html"].etag,
            second.files["index.html"].etag
        );
        assert_eq!(
            second.serve(request(&path), true).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            first.files["js/child.js"].bytes.as_ref(),
            b"export const value = 1;"
        );
        assert_eq!(
            first.serve(request("/_assets/old/no-file"), true).status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn public_snapshot_contains_only_public_and_allowlisted_shared_files() {
        let assets = StaticAssets::load(true);
        for path in [
            "shared/js/api.js",
            "shared/js/settings.js",
            "shared/index.html",
            "../dist/js/api.js",
        ] {
            assert!(!assets.files.contains_key(path));
        }
        assert!(assets.files.contains_key("shared/js/dom.js"));
        assert!(assets.files.contains_key("js/main.js"));
    }
}
