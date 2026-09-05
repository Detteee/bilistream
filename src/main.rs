// Hide console window on Windows in release mode
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    bilistream::runtime::cli_main().await
}
