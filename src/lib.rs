pub mod config;
pub mod plugins;
pub mod webui;

// Re-export for convenience
pub use webui::api::{
    add_log_line, update_status_cache, BiliStatus, StatusData, TwStatus, YtStatus,
};

#[cfg(target_os = "windows")]
pub mod windows_deps;
// Re-export anything that needs to be public
pub use config::{load_config, Config};
