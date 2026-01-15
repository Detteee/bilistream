use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Router,
};
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use super::api;

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

pub async fn start_webui(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize log buffer
    api::init_log_buffer();

    // API router
    let api_router = Router::new()
        .route("/health", get(health_check))
        .route("/version", get(api::get_version))
        .route("/status", get(api::get_status))
        .route("/config", get(api::get_config).post(api::update_config))
        .route("/start", post(api::start_stream))
        .route("/stop", post(api::stop_stream))
        .route("/restart", post(api::restart_stream))
        .route("/danmaku", post(api::send_danmaku))
        .route("/cover", post(api::update_cover))
        .route("/area", post(api::update_area))
        .route("/title", post(api::update_title))
        .route("/channels", get(api::get_channels))
        .route("/areas", get(api::get_areas))
        .route("/channel", post(api::update_channel))
        .route("/setup-status", get(api::check_setup))
        .route("/logs", get(api::get_logs_endpoint))
        .route("/setup/save-config", post(api::save_setup_config))
        .route("/setup/login-status", get(api::check_login_status))
        .route("/setup/login", post(api::trigger_login))
        .route("/setup/qrcode", get(api::get_qr_code))
        .route("/setup/poll-login", post(api::poll_login))
        .route("/update/check", get(api::check_updates))
        .route("/update/download", post(api::download_update))
        .route("/deps/status", get(api::get_deps_status))
        .route("/holodex/streams", get(api::api_get_holodex_streams))
        .route("/holodex/switch", post(api::switch_to_holodex_stream))
        .route("/refresh/youtube", get(api::refresh_youtube_status))
        .route("/refresh/twitch", get(api::refresh_twitch_status))
        .route("/banned-keywords", get(api::get_banned_keywords))
        .route("/banned-keywords", post(api::update_banned_keywords))
        .route("/manage/areas", get(api::get_areas_manage))
        .route("/manage/areas", post(api::add_area))
        .route("/manage/areas/:id", delete(api::delete_area))
        .route("/manage/channels", get(api::get_channels_manage))
        .route("/manage/channels", post(api::add_channel))
        .route("/manage/channels", put(api::update_channel_manage))
        .route("/manage/channels/:name", delete(api::delete_channel));

    // Main app with API routes and static files
    let app = Router::new()
        .nest("/api", api_router)
        .fallback_service(ServeDir::new("webui/dist"))
        .layer(CorsLayer::permissive());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    println!("\n🌐 Web UI 服务已启动");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("📍 本地访问:     http://localhost:{}", port);
    println!("📍 本地访问:     http://127.0.0.1:{}", port);

    // Try to get local network IP
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(local_addr) = socket.local_addr() {
                let ip = local_addr.ip();
                if !ip.is_loopback() {
                    println!("📍 局域网访问:   http://{}:{}", ip, port);
                }
            }
        }
    }

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("💡 提示: 在浏览器中打开上述任一地址访问\n");

    // tracing::info!("Web UI listening on 0.0.0.0:{}", port);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
