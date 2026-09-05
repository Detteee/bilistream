use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

const PORT: u16 = 3150;

#[derive(Default)]
struct BackendState(tokio::sync::Mutex<Option<bilistream::runtime::BackendRuntime>>);

fn quit(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(backend) = app.state::<BackendState>().0.lock().await.take() {
            backend.shutdown().await;
        }
        app.exit(0);
    });
}

// Force XWayland backend to avoid Wayland protocol errors with WebKitGTK
#[cfg(target_os = "linux")]
fn init_display_backend() {
    if std::env::var("GDK_BACKEND").is_err() {
        std::env::set_var("GDK_BACKEND", "x11");
    }
    // Disable GPU/hardware acceleration to avoid GBM buffer errors
    std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
}

fn open_window(app: &AppHandle) {
    let url = format!("http://localhost:{}", PORT);
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    } else {
        let Ok(url) = url.parse() else {
            return;
        };
        let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
            .title("Bilistream")
            .inner_size(1280.0, 860.0)
            .build();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    bilistream::install_crypto_provider();

    #[cfg(target_os = "linux")]
    init_display_backend();

    tauri::Builder::default()
        .manage(BackendState::default())
        .plugin(tauri_plugin_shell::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Hide window instead of closing — exit only via tray menu
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .setup(|app| {
            // Build tray menu
            let open_item = MenuItem::with_id(app, "open", "打开控制面板", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &quit_item])?;

            // Build tray icon
            let icon_bytes = include_bytes!("../icons/icon.png");
            let icon = Image::from_bytes(icon_bytes)?;

            TrayIconBuilder::new()
                .icon(icon)
                .tooltip("Bilistream")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => open_window(app),
                    "quit" => quit(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        open_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // Share the full backend and open only after the listener binds.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<BackendState>();
                let mut backend = state.0.lock().await;
                match bilistream::runtime::BackendRuntime::start(
                    PORT,
                    "error",
                    bilistream::AppState::new().install(),
                )
                .await
                {
                    Ok(runtime) => {
                        *backend = Some(runtime);
                        open_window(&handle);
                    }
                    Err(error) => eprintln!("Backend startup failed: {error}"),
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
