pub mod app_state;
pub mod config;
pub mod deps;
pub mod plugins;
pub mod tray;
pub mod updater;
pub mod webui;
pub use app_state::AppState;
pub use webui::server::start_webui;

// Re-export for convenience
pub use webui::state::{
    add_log_line, update_status_cache, BiliStatus, StatusData, TwStatus, YtStatus,
};

// Re-export anything that needs to be public
pub use config::{load_config, Config};

/// Installs the process-wide rustls crypto backend.
///
/// reqwest is built with `rustls-no-provider` so the backend can be `ring`
/// rather than `aws-lc-rs`, which needs cmake and nasm and whose symbol count
/// broke the mingw link. The trade-off is that `ClientBuilder::build` panics
/// until a provider is installed, so every entry point must call this before
/// any request — as must any test that constructs a client.
pub fn install_crypto_provider() {
    // An error means a provider is already installed, which is the desired end
    // state either way.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    #[test]
    fn crypto_provider_is_available_for_tls_config() {
        super::install_crypto_provider();
        // Panics if no process-wide provider is installed, which is exactly the
        // failure mode `rustls-no-provider` introduces.
        let _ = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        // Calling twice must stay harmless; entry points may both call it.
        super::install_crypto_provider();
    }
}
