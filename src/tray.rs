// System tray module - Opens WebUI in browser

// Linux/macOS implementation using ksni
#[cfg(not(target_os = "windows"))]
pub fn run_tray(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    use ksni;

    struct BiliTray {
        port: u16,
    }

    impl ksni::Tray for BiliTray {
        fn id(&self) -> String {
            "bilistream".to_string()
        }

        fn title(&self) -> String {
            "Bilistream".to_string()
        }

        fn icon_name(&self) -> String {
            "media-playback-start".to_string()
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::*;
            vec![
                StandardItem {
                    label: "打开控制面板".to_string(),
                    activate: Box::new(|this: &mut Self| {
                        let url = format!("http://localhost:{}", this.port);
                        if let Err(e) = open::that(&url) {
                            eprintln!("Failed to open browser: {}", e);
                        }
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "退出".to_string(),
                    activate: Box::new(|_| {
                        std::process::exit(0);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            let url = format!("http://localhost:{}", self.port);
            if let Err(e) = open::that(&url) {
                eprintln!("Failed to open browser: {}", e);
            }
        }
    }

    let tray = BiliTray { port };
    let service = ksni::TrayService::new(tray);
    service.spawn();

    tracing::info!("✅ 系统托盘已启动");

    // Auto-open browser on startup
    let url = format!("http://localhost:{}", port);
    tracing::info!("🌐 正在打开浏览器: {}", url);
    if let Err(e) = open::that(&url) {
        tracing::warn!("⚠️ 无法自动打开浏览器: {}", e);
        tracing::info!("💡 请手动访问: {}", url);
    } else {
        tracing::info!("✅ 浏览器已打开");
    }

    tracing::info!("💡 点击托盘图标可重新打开控制面板");

    // Keep main thread alive indefinitely
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

// Windows implementation - system tray with native Windows API
#[cfg(target_os = "windows")]
pub fn run_tray(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc;
    use trayicon::{Icon, MenuBuilder, TrayIconBuilder};

    #[derive(Copy, Clone, Eq, PartialEq, Debug)]
    enum Events {
        ClickTrayIcon,
        OpenPanel,
        Exit,
    }

    let (tx, rx) = mpsc::channel::<Events>();
    let tx_clone = tx.clone();

    // Create tray icon with menu (icon is embedded at compile time)
    let icon_data = include_bytes!("../icon.ico");
    let icon = Icon::from_buffer(icon_data, None, None)?;

    let _tray_icon = TrayIconBuilder::new()
        .sender(move |e: &Events| {
            let _ = tx_clone.send(*e);
        })
        .icon(icon)
        .tooltip("Bilistream - 左键打开控制面板，右键显示菜单")
        .on_click(Events::ClickTrayIcon)
        .on_double_click(Events::OpenPanel)
        .menu(
            MenuBuilder::new()
                .item("打开控制面板", Events::OpenPanel)
                .separator()
                .item("退出", Events::Exit),
        )
        .build()?;

    tracing::info!("✅ 系统托盘已启动");

    // Auto-open browser on startup
    let url = format!("http://localhost:{}", port);
    tracing::info!("🌐 正在打开浏览器: {}", url);
    if let Err(e) = open::that(&url) {
        tracing::warn!("⚠️ 无法自动打开浏览器: {}", e);
        tracing::info!("💡 请手动访问: {}", url);
    } else {
        tracing::info!("✅ 浏览器已打开");
    }

    tracing::info!("💡 点击托盘图标打开控制面板，右键显示菜单");

    // Spawn event handler in separate thread
    std::thread::spawn(move || loop {
        match rx.recv() {
            Ok(Events::ClickTrayIcon) | Ok(Events::OpenPanel) => {
                let url = format!("http://localhost:{}", port);
                let _ = open::that(&url);
            }
            Ok(Events::Exit) => {
                std::process::exit(0);
            }
            Err(_) => break,
        }
    });

    // Windows message loop - required for tray icon events
    use std::ptr;
    use winapi::um::winuser::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(())
}
