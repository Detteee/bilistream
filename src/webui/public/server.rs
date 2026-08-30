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
use tower_http::set_header::SetResponseHeaderLayer;

use super::snapshot::{current_public_status, SNAPSHOT_TTL};

/// Nothing here accepts a body; anything larger is refused before it is read.
const MAX_BODY_BYTES: usize = 4 * 1024;

/// Areas change when the operator edits areas.json, which is rare, so the edge
/// may hold them far longer than the status payload.
const AREAS_MAX_AGE_SECS: u64 = 300;

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

    if let Some(requested) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if requested == etag {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
        }
    }

    (
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (
                header::CONTENT_TYPE,
                "application/json; charset=utf-8".to_string(),
            ),
            // The tunnel in front is the thing that absorbs an audience:
            // letting the edge hold the payload for as long as the snapshot
            // lives collapses any number of viewers into one origin request
            // per window.
            (header::CACHE_CONTROL, cache_control(SNAPSHOT_TTL.as_secs())),
        ],
        body,
    )
        .into_response()
}

/// Area ids, names and aliases: the page needs the aliases to build the
/// danmaku command. The banned keyword lists in the same file stay private.
async fn public_areas() -> Response {
    let Ok(exe) = std::env::current_exe() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "areas unavailable").into_response();
    };
    let Ok(content) = tokio::fs::read_to_string(exe.with_file_name("areas.json")).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "areas unavailable").into_response();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "areas unavailable").into_response();
    };

    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, cache_control(AREAS_MAX_AGE_SECS))],
        Json(public_areas_from_json(&parsed)),
    )
        .into_response()
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
    parsed["areas"]
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
                .collect()
        })
        .unwrap_or_default()
}

pub fn public_router() -> Router {
    let api = Router::new()
        .route("/status", get(public_status))
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
        ));

    Router::new()
        .route("/health", get(health))
        .nest("/api/public", api)
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(CompressionLayer::new())
        .layer(security_headers)
}

/// Watches config and keeps the listener matching it, so changing the serving
/// node from the panel takes effect without a restart on either node.
pub fn start_public_status_supervisor() {
    tokio::spawn(async {
        let mut running: Option<(SocketAddr, tokio::sync::oneshot::Sender<()>)> = None;

        loop {
            let desired = desired_listener().await;

            match (&running, desired) {
                (Some((addr, _)), Some(wanted)) if *addr == wanted => {}
                (_, Some(wanted)) => {
                    if let Some((addr, stop)) = running.take() {
                        tracing::info!("公开状态页停止监听 {}", addr);
                        let _ = stop.send(());
                    }
                    match spawn_listener(wanted).await {
                        Ok(stop) => {
                            tracing::info!("🌐 公开状态页已启动: http://{}", wanted);
                            running = Some((wanted, stop));
                        }
                        Err(e) => tracing::error!("公开状态页启动失败 ({}): {}", wanted, e),
                    }
                }
                (Some(_), None) => {
                    if let Some((addr, stop)) = running.take() {
                        tracing::info!("公开状态页停止监听 {}", addr);
                        let _ = stop.send(());
                    }
                }
                (None, None) => {}
            }

            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}

/// The address this node should be listening on, if any.
async fn desired_listener() -> Option<SocketAddr> {
    let cfg = crate::config::load_config().await.ok()?;
    let public = &cfg.cluster.public_status;

    if !public.runs_on(&cfg.cluster.node_id) {
        return None;
    }
    if let Err(e) = public.validate() {
        tracing::warn!("公开状态页配置无效，未启动: {}", e);
        return None;
    }

    let bind = crate::webui::parse_bind(&public.bind).ok()?;
    Some(SocketAddr::new(bind, public.port))
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

        for path in ["/health", "/api/public/status", "/api/public/areas"] {
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

    #[test]
    fn cache_control_lets_the_edge_absorb_the_audience() {
        assert_eq!(
            cache_control(5),
            "public, max-age=5, stale-while-revalidate=30"
        );
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
    fn a_file_without_areas_yields_nothing_rather_than_erroring() {
        let parsed = serde_json::json!({ "banned_keywords": ["asmr"] });
        assert!(public_areas_from_json(&parsed).is_empty());
    }
}
