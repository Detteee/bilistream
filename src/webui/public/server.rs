//! The public status listener.
//!
//! A separate axum server on its own port rather than a path on the admin
//! app: the admin server can stay on loopback, no admin route can leak through
//! a fallback, and this one gets its own limits and headers. Every route here
//! is a GET — there is no mutation endpoint to authorise, which is why the page
//! needs no login.

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use super::snapshot::{current_public_status, SNAPSHOT_TTL};
use super::streams::{current_public_streams, start_streams_watch};
use super::thumbnails::read_thumbnail;
use crate::webui::server::static_asset_dir;

/// Nothing here accepts a body; anything larger is refused before it is read.
const MAX_BODY_BYTES: usize = 4 * 1024;

/// Areas change when the operator edits areas.json, which is rare, so the edge
/// may hold them far longer than the status payload.
const AREAS_MAX_AGE_SECS: u64 = 300;

/// Viewers poll every 30s; the edge never holds a list longer than that.
const STREAMS_MAX_AGE_SECS: u64 = 30;

#[derive(Serialize)]
struct PublicArea {
    id: u64,
    name: String,
    aliases: Vec<String>,
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// The cached status payload, with an ETag so repeat polls settle for a 304.
async fn public_status(headers: HeaderMap) -> Response {
    let Some((body, etag)) = current_public_status().await else {
        return (StatusCode::SERVICE_UNAVAILABLE, "status unavailable").into_response();
    };

    public_json_response(&headers, body, etag, SNAPSHOT_TTL.as_secs())
}

/// The cached stream list. Rebuilt from the channels list, never on request,
/// so viewer traffic cannot reach Holodex or Google at all.
async fn public_streams(headers: HeaderMap) -> Response {
    let Some((body, etag)) = current_public_streams().await else {
        // Before the first successful fetch there is nothing to show; say so
        // rather than serving an empty list that reads as "nobody is live".
        return (StatusCode::SERVICE_UNAVAILABLE, "streams unavailable").into_response();
    };

    public_json_response(&headers, body, etag, STREAMS_MAX_AGE_SECS)
}

fn public_json_response(
    headers: &HeaderMap,
    body: axum::body::Bytes,
    etag: String,
    max_age: u64,
) -> Response {
    let cache_headers = [
        (header::ETAG, etag.clone()),
        (header::CACHE_CONTROL, cache_control(max_age)),
    ];
    if not_modified(headers, &etag) {
        // A 304 must retain the same cache policy as its representation.
        // Otherwise the fallback no-store header disables conditional caching.
        return (StatusCode::NOT_MODIFIED, cache_headers).into_response();
    }
    (
        StatusCode::OK,
        cache_headers,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Area ids, names and aliases: the page needs the aliases to build the
/// danmaku command. The banned keyword lists in the same file stay private.
async fn public_areas() -> Response {
    let Ok(parsed) = crate::storage::read_json::<serde_json::Value>("areas.json") else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "areas unavailable").into_response();
    };

    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, cache_control(AREAS_MAX_AGE_SECS))],
        Json(public_areas_from_json(&parsed)),
    )
        .into_response()
}

/// A cached thumbnail. Keys are derived from the upstream url, so the bytes
/// behind one never change and may be held indefinitely.
async fn public_thumbnail(axum::extract::Path(key): axum::extract::Path<String>) -> Response {
    let Some((bytes, content_type)) = read_thumbnail(&key).await else {
        return (StatusCode::NOT_FOUND, "unknown thumbnail").into_response();
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CACHE_CONTROL,
                "public, max-age=86400, immutable".to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    let expected = etag.strip_prefix("W/").unwrap_or(etag);
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|requested| {
            requested.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == expected
            })
        })
}

fn cache_control(max_age: u64) -> String {
    // stale-while-revalidate keeps the edge answering during the refresh, so a
    // slow rebuild never turns into a burst of origin traffic.
    format!(
        "public, max-age={max_age}, stale-while-revalidate={}",
        max_age * 6
    )
}

fn public_areas_from_json(parsed: &serde_json::Value) -> Vec<PublicArea> {
    let mut areas = parsed["areas"]
        .as_array()
        .map(|areas| {
            areas
                .iter()
                .filter_map(|area| {
                    Some(PublicArea {
                        id: area["id"].as_u64()?,
                        name: area["name"].as_str()?.to_string(),
                        aliases: area["aliases"]
                            .as_array()
                            .map(|aliases| {
                                aliases
                                    .iter()
                                    .filter_map(|alias| alias.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    areas.sort_by_key(|area| u8::from(area.id != crate::plugins::DEFAULT_AREA_ID));
    areas
}

/// Assets shared with the dashboard, mounted file by file rather than as a
/// directory: the public port must not be able to serve the dashboard's own
/// markup or its admin-only modules.
fn shared_assets() -> Router {
    let assets = static_asset_dir("webui/dist");
    Router::new()
        .route_service("/styles.css", ServeFile::new(assets.join("styles.css")))
        .route_service("/js/dom.js", ServeFile::new(assets.join("js/dom.js")))
        .route_service("/js/format.js", ServeFile::new(assets.join("js/format.js")))
        .route_service("/js/dialog.js", ServeFile::new(assets.join("js/dialog.js")))
        .route_service(
            "/js/cluster-health.js",
            ServeFile::new(assets.join("js/cluster-health.js")),
        )
        .route_service(
            "/js/cluster-network.js",
            ServeFile::new(assets.join("js/cluster-network.js")),
        )
        .route_service(
            "/js/status-cards.js",
            ServeFile::new(assets.join("js/status-cards.js")),
        )
}

pub fn public_router() -> Router {
    let api = Router::new()
        .route("/status", get(public_status))
        .route("/streams", get(public_streams))
        .route("/areas", get(public_areas));

    let security_headers = ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; img-src 'self' data:; style-src 'self'; \
                 script-src 'self'; frame-ancestors 'none'; base-uri 'none'",
            ),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ));

    // No not_found_service: the page is a single document with no client-side
    // routing, and falling back to it would answer an admin path with 200
    // instead of the 404 that says the route does not exist here.
    let page = ServeDir::new(static_asset_dir("webui/public-dist"));

    Router::new()
        .route("/health", get(health))
        .route("/t/{key}", get(public_thumbnail))
        .nest("/api/public", api)
        .nest("/shared", shared_assets())
        .fallback_service(page)
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(CompressionLayer::new())
        .layer(security_headers)
}

/// Watches config and keeps the listener matching it, so changing the serving
/// node from the panel takes effect without a restart on either node.
pub struct PublicStatusSupervisor(tokio::task::JoinHandle<()>);

impl Drop for PublicStatusSupervisor {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn start_public_status_supervisor() -> PublicStatusSupervisor {
    PublicStatusSupervisor(tokio::spawn(async {
        let mut running: Option<RunningListener> = None;

        loop {
            let desired = desired_listener().await;

            match (&running, desired) {
                // Already serving exactly this; leave the timer running.
                (Some(current), Some(wanted)) if current.matches(&wanted) => {}
                (_, Some(wanted)) => {
                    stop_listener(running.take());
                    match spawn_listener(wanted.addr).await {
                        Ok(stop) => {
                            tracing::info!("🌐 公开状态页已启动: http://{}", wanted.addr);
                            running = Some(RunningListener {
                                addr: wanted.addr,
                                stop_server: stop,
                                stop_refresh: start_streams_watch(),
                            });
                        }
                        Err(e) => tracing::error!("公开状态页启动失败 ({}): {}", wanted.addr, e),
                    }
                }
                (Some(_), None) => stop_listener(running.take()),
                (None, None) => {}
            }

            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }))
}

struct RunningListener {
    addr: SocketAddr,
    stop_server: tokio::sync::oneshot::Sender<()>,
    stop_refresh: tokio::sync::oneshot::Sender<()>,
}

impl RunningListener {
    fn matches(&self, wanted: &DesiredListener) -> bool {
        self.addr == wanted.addr
    }
}

struct DesiredListener {
    addr: SocketAddr,
}

fn stop_listener(running: Option<RunningListener>) {
    if let Some(running) = running {
        tracing::info!("公开状态页停止监听 {}", running.addr);
        let _ = running.stop_server.send(());
        let _ = running.stop_refresh.send(());
    }
}

/// What this node should be running, if anything.
async fn desired_listener() -> Option<DesiredListener> {
    let cfg = crate::config::load_config().await.ok()?;
    desired_listener_for_config(&cfg)
}

fn desired_listener_for_config(cfg: &crate::config::Config) -> Option<DesiredListener> {
    let public = &cfg.cluster.public_status;

    if !public.runs_on(&cfg.cluster.node_id) {
        return None;
    }
    if let Err(e) = public.validate() {
        tracing::warn!("公开状态页配置无效，未启动: {}", e);
        return None;
    }

    let bind = crate::webui::parse_bind(&public.bind).ok()?;
    Some(DesiredListener {
        addr: SocketAddr::new(bind, public.port),
    })
}

async fn spawn_listener(
    addr: SocketAddr,
) -> Result<tokio::sync::oneshot::Sender<()>, std::io::Error> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let served = axum::serve(listener, public_router())
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await;

        if let Err(e) = served {
            tracing::error!("公开状态页监听结束: {}", e);
        }
    });

    Ok(stop_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_public_page_needs_no_cluster_or_peers() {
        let mut cfg = crate::cluster::tests::test_config("home", 0);
        cfg.cluster.enabled = false;
        cfg.cluster.peers.clear();
        cfg.cluster.public_status.node_id = "home".into();
        let desired = desired_listener_for_config(&cfg).unwrap();
        assert_eq!(desired.addr.port(), cfg.cluster.public_status.port);
        assert!(desired.addr.ip().is_loopback());
        cfg.cluster.public_status.node_id.clear();
        assert!(desired_listener_for_config(&cfg).is_none());
        cfg.cluster.public_status.node_id = "other".into();
        assert!(desired_listener_for_config(&cfg).is_none());
        cfg.cluster.public_status.node_id = "home".into();
        cfg.cluster.public_status.port = 0;
        assert!(desired_listener_for_config(&cfg).is_none());
    }

    #[test]
    fn conditional_get_accepts_weak_validators_and_lists() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"old\", W/\"current\""),
        );
        assert!(not_modified(&headers, "\"current\""));
        assert!(not_modified(&headers, "W/\"current\""));
        assert!(!not_modified(&headers, "W/\"changed\""));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(not_modified(&headers, "W/\"current\""));
    }

    async fn serve_for_test() -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        // reqwest::Client::build panics without one; see install_crypto_provider.
        crate::install_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let _ = axum::serve(listener, public_router())
                .with_graceful_shutdown(async {
                    let _ = stop_rx.await;
                })
                .await;
        });

        (addr, stop_tx)
    }

    /// The admin API must not be reachable here at all. Absence of the route
    /// is the guarantee, not a permission check on a shared one.
    #[tokio::test]
    async fn the_listener_exposes_no_admin_route() {
        let (addr, stop) = serve_for_test().await;
        let client = reqwest::Client::new();

        for path in [
            "/api/config",
            "/api/logs",
            "/api/status",
            "/api/channels",
            "/api/cluster/status",
            "/api/start",
            "/api/danmaku",
            "/api/crop/youtube",
            "/api/setup/qrcode",
        ] {
            let url = format!("http://{addr}{path}");
            let response = client.get(&url).send().await.unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND,
                "{path} should not exist on the public listener"
            );

            let posted = client.post(&url).send().await.unwrap();
            assert!(
                posted.status() == reqwest::StatusCode::NOT_FOUND
                    || posted.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED,
                "{path} accepted a POST"
            );
        }

        let _ = stop.send(());
    }

    #[tokio::test]
    async fn health_answers_and_carries_the_security_headers() {
        let (addr, stop) = serve_for_test().await;

        let response = reqwest::get(format!("http://{addr}/health")).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let headers = response.headers();
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
        assert!(headers
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));

        let _ = stop.send(());
    }

    /// Mutations are what would need an identity, and there are none.
    #[tokio::test]
    async fn every_declared_route_is_read_only() {
        let (addr, stop) = serve_for_test().await;
        let client = reqwest::Client::new();

        for path in [
            "/health",
            "/api/public/status",
            "/api/public/streams",
            "/api/public/areas",
            "/t/deadbeef",
        ] {
            let response = client
                .post(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::METHOD_NOT_ALLOWED,
                "{path} accepted a POST"
            );
        }

        let _ = stop.send(());
    }

    /// Before the first successful fetch, an empty list would read as "nobody
    /// is live" rather than "not loaded yet".
    #[tokio::test]
    async fn streams_report_unavailable_rather_than_empty_before_the_first_fetch() {
        let (addr, stop) = serve_for_test().await;

        let response = reqwest::get(format!("http://{addr}/api/public/streams"))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_ne!(response.text().await.unwrap().trim(), "[]");

        let _ = stop.send(());
    }

    /// A key the node never minted must not reach the filesystem.
    #[tokio::test]
    async fn unknown_thumbnails_are_not_found() {
        let (addr, stop) = serve_for_test().await;
        let client = reqwest::Client::new();

        // "..", unencoded, is normalized away by the client before it is
        // sent, so it would test nothing here; the encoded form does reach us.
        for key in [
            "deadbeef",
            "zzzz",
            "%2e%2e%2fetc%2fpasswd",
            "..%2f..%2fconfig.json",
        ] {
            let response = client
                .get(format!("http://{addr}/t/{key}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND,
                "/t/{key} should not resolve"
            );
        }

        let _ = stop.send(());
    }

    /// Shared assets are mounted one by one, so the dashboard's own markup and
    /// its admin-only modules must not be reachable through /shared.
    #[tokio::test]
    async fn the_public_port_serves_only_the_named_shared_assets() {
        let (addr, stop) = serve_for_test().await;
        let client = reqwest::Client::new();

        for path in [
            "/shared/styles.css",
            "/shared/js/status-cards.js",
            "/shared/js/dialog.js",
            "/shared/js/cluster-network.js",
            "/shared/js/cluster-health.js",
        ] {
            let response = client
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK, "{path} missing");
        }

        for path in [
            "/shared/index.html",
            "/shared/js/overview.js",
            "/shared/js/cluster.js",
            "/shared/js/settings.js",
            "/shared/js/api.js",
            "/js/overview.js",
        ] {
            let response = client
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND,
                "{path} should not be served publicly"
            );
        }

        let _ = stop.send(());
    }

    #[tokio::test]
    async fn the_page_itself_is_served() {
        let (addr, stop) = serve_for_test().await;

        let response = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.text().await.unwrap();

        assert!(body.contains("id=\"holodex-streams\""));
        assert!(body.contains("id=\"cluster-node-list\""));
        // No dashboard chrome: no tab bar and no editing controls.
        assert!(!body.contains("data-view=\"settings\""));
        assert!(!body.contains("id=\"tab-logs\""));
        assert!(!body.contains("crop"));

        let _ = stop.send(());
    }

    /// HTML/CSS/JS are iterated often; without this, a tunnel in front keeps
    /// serving the previous streams.js after index.html already changed.
    #[tokio::test]
    async fn static_page_assets_are_not_cached() {
        let (addr, stop) = serve_for_test().await;
        let client = reqwest::Client::new();

        for path in [
            "/",
            "/public.css",
            "/js/main.js",
            "/shared/styles.css",
            "/icon.png",
            "/icon-blue.png",
        ] {
            let response = client
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
            assert_eq!(
                response.headers().get("cache-control").unwrap(),
                "no-store",
                "{path} must revalidate"
            );
        }

        let _ = stop.send(());
    }

    #[test]
    fn cache_control_lets_the_edge_absorb_the_audience() {
        assert_eq!(
            cache_control(5),
            "public, max-age=5, stale-while-revalidate=30"
        );
    }

    #[tokio::test]
    async fn conditional_public_responses_retain_the_representation_cache_policy() {
        let mut headers = HeaderMap::new();
        let body = axum::body::Bytes::from_static(b"{}");
        let full = public_json_response(&headers, body.clone(), "\"status\"".into(), 5);
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("W/\"status\""),
        );
        let unchanged = public_json_response(&headers, body, "\"status\"".into(), 5);
        assert_eq!(full.status(), StatusCode::OK);
        assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            full.headers()[header::CACHE_CONTROL],
            unchanged.headers()[header::CACHE_CONTROL]
        );
        assert_eq!(
            full.headers()[header::ETAG],
            unchanged.headers()[header::ETAG]
        );
        assert!(axum::body::to_bytes(unchanged.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn public_areas_keep_only_what_the_command_needs() {
        let parsed = serde_json::json!({
            "areas": [
                { "id": 86, "name": "英雄联盟", "aliases": ["lol"], "title_keywords": ["league"] },
                { "id": 80, "name": "吃鸡行动", "aliases": ["吃鸡"] }
            ],
            "banned_keywords": ["asmr"],
            "streaming_banned_keywords": ["gta"]
        });

        let areas = public_areas_from_json(&parsed);
        let json = serde_json::to_string(&areas).unwrap();

        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].aliases, vec!["lol".to_string()]);
        assert!(!json.contains("title_keywords"));
        assert!(!json.contains("banned_keywords"));
        assert!(!json.contains("asmr"));
        assert!(!json.contains("gta"));
    }

    #[test]
    fn areas_without_an_id_or_name_are_skipped() {
        let parsed = serde_json::json!({
            "areas": [
                { "name": "缺少 id" },
                { "id": 80 },
                { "id": 86, "name": "英雄联盟" }
            ]
        });

        let areas = public_areas_from_json(&parsed);
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].id, 86);
        assert!(areas[0].aliases.is_empty());
    }

    #[test]
    fn the_catchall_area_is_listed_first() {
        let parsed = serde_json::json!({
            "areas": [
                { "id": 86, "name": "英雄联盟" },
                { "id": 235, "name": "其他单机" },
                { "id": 252, "name": "逃离塔科夫" }
            ]
        });

        let areas = public_areas_from_json(&parsed);
        assert_eq!(
            areas.iter().map(|area| area.id).collect::<Vec<_>>(),
            vec![235, 86, 252]
        );
    }

    #[test]
    fn a_file_without_areas_yields_nothing_rather_than_erroring() {
        let parsed = serde_json::json!({ "banned_keywords": ["asmr"] });
        assert!(public_areas_from_json(&parsed).is_empty());
    }
}
