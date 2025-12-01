pub mod config;
pub mod deps;
pub mod plugins;
pub mod tray;
pub mod updater;
pub mod webui;

// Re-export for convenience
pub use webui::api::{
    add_log_line, update_status_cache, BiliStatus, StatusData, TwStatus, YtStatus,
};

// Re-export anything that needs to be public
pub use config::{load_config, Config};
