mod config;
mod credentials;
mod db;
mod immich;
mod sync;

use config::{load_config, save_config, AppConfig};
use credentials::{clear_api_key, get_api_key, set_api_key};
use immich::{fetch_server_storage_cached, verify_connection};
use serde::{Deserialize, Serialize};
use sync::{refresh_tray_icon, SyncController, SyncStatusDto};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WindowEvent};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDto {
    pub server_url: String,
    pub watch_paths: Vec<String>,
    pub sync_enabled: bool,
    pub minimize_to_tray: bool,
    pub has_api_key: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSettingsInput {
    pub server_url: String,
    pub watch_paths: Vec<String>,
    pub sync_enabled: bool,
    pub minimize_to_tray: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfoDto {
    pub server_disk_available_raw: Option<u64>,
    pub server_disk_usage_percentage: Option<f64>,
    pub server_disk_available_human: Option<String>,
    /// API key lacks `server.storage` (GET /api/server/storage returned 401/403).
    pub server_storage_forbidden: bool,
    pub server_error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestConnectionInput {
    pub server_url: String,
    /// When set (e.g. from the form), test with this key instead of the saved keyring value.
    pub api_key: Option<String>,
}

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Result<SettingsDto, String> {
    let c = load_config(&app)?;
    Ok(SettingsDto {
        server_url: c.server_url,
        watch_paths: c.watch_paths,
        sync_enabled: c.sync_enabled,
        minimize_to_tray: c.minimize_to_tray,
        has_api_key: get_api_key().ok().flatten().is_some(),
    })
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    sync: tauri::State<'_, Arc<SyncController>>,
    tray_flag: tauri::State<'_, TrayMinimizeFlag>,
    input: SaveSettingsInput,
) -> Result<(), String> {
    tray_flag.set(input.minimize_to_tray);
    let config = AppConfig {
        server_url: input.server_url,
        watch_paths: input.watch_paths,
        sync_enabled: input.sync_enabled,
        minimize_to_tray: input.minimize_to_tray,
    };
    save_config(&app, &config)?;
    sync.invalidate_library_cache();
    if get_api_key().ok().flatten().is_some() {
        sync.clear_last_error();
    }
    Ok(())
}

#[tauri::command]
fn set_api_key_cmd(
    key: String,
    sync: tauri::State<'_, Arc<SyncController>>,
) -> Result<(), String> {
    set_api_key(&key)?;
    sync.clear_last_error();
    Ok(())
}

#[tauri::command]
fn clear_api_key_cmd() -> Result<(), String> {
    clear_api_key()
}

#[tauri::command]
fn test_connection(input: TestConnectionInput) -> Result<String, String> {
    let key = match input
        .api_key
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(k) => k.to_string(),
        None => get_api_key()
            .ok()
            .flatten()
            .ok_or("No API key configured. Paste a key or save settings first.")?,
    };
    verify_connection(&input.server_url, &key)
}

#[tauri::command]
fn get_sync_status(
    app: tauri::AppHandle,
    sync: tauri::State<'_, Arc<SyncController>>,
) -> Result<SyncStatusDto, String> {
    sync.status_with_library(&app)
}

#[tauri::command]
fn get_storage_info(app: tauri::AppHandle) -> Result<StorageInfoDto, String> {
    let cfg = load_config(&app)?;

    let api_key = get_api_key().ok().flatten();
    let Some(ref key) = api_key else {
        return Ok(StorageInfoDto {
            server_disk_available_raw: None,
            server_disk_usage_percentage: None,
            server_disk_available_human: None,
            server_storage_forbidden: false,
            server_error: None,
        });
    };

    match fetch_server_storage_cached(&cfg.server_url, key) {
        Ok(None) => Ok(StorageInfoDto {
            server_disk_available_raw: None,
            server_disk_usage_percentage: None,
            server_disk_available_human: None,
            server_storage_forbidden: true,
            server_error: None,
        }),
        Ok(Some(s)) => Ok(StorageInfoDto {
            server_disk_available_raw: Some(s.disk_available_raw),
            server_disk_usage_percentage: Some(s.disk_usage_percentage),
            server_disk_available_human: if s.disk_available.is_empty() {
                None
            } else {
                Some(s.disk_available)
            },
            server_storage_forbidden: false,
            server_error: None,
        }),
        Err(e) => Ok(StorageInfoDto {
            server_disk_available_raw: None,
            server_disk_usage_percentage: None,
            server_disk_available_human: None,
            server_storage_forbidden: false,
            server_error: Some(e),
        }),
    }
}

#[tauri::command]
fn start_sync(
    app: tauri::AppHandle,
    sync: tauri::State<'_, Arc<SyncController>>,
) -> Result<(), String> {
    let c = Arc::clone(&sync);
    c.start(&app, c.clone())
}

#[tauri::command]
fn stop_sync(
    _app: tauri::AppHandle,
    sync: tauri::State<'_, Arc<SyncController>>,
) -> Result<(), String> {
    sync.stop();
    Ok(())
}

/// Live copy of `minimize_to_tray` for window handlers (avoid disk reads on every resize).
#[derive(Clone)]
struct TrayMinimizeFlag(Arc<AtomicBool>);

impl TrayMinimizeFlag {
    fn new(initial: bool) -> Self {
        Self(Arc::new(AtomicBool::new(initial)))
    }

    fn set(&self, v: bool) {
        self.0.store(v, Ordering::Relaxed);
    }

    fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

fn show_and_focus_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let menu = MenuBuilder::new(app)
        .text("open", "Open Immich Sync")
        .separator()
        .text("start_sync", "Start sync")
        .text("stop_sync", "Stop sync")
        .separator()
        .text("quit", "Exit")
        .build()?;

    let icon = app
        .default_window_icon()
        .cloned()
        .or_else(|| {
            tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png")).ok()
        })
        .ok_or("no icon for tray (bundle icons / 32x32.png missing)")?;

    let tray = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Immich Sync")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let id = event.id().as_ref();
            if id == "open" {
                show_and_focus_main(app);
            } else if id == "start_sync" {
                if let Some(sync) = app.try_state::<Arc<SyncController>>() {
                    let sync = Arc::clone(&sync);
                    let app = app.clone();
                    std::thread::spawn(move || {
                        let c = Arc::clone(&sync);
                        let _ = c.start(&app, c.clone());
                    });
                }
            } else if id == "stop_sync" {
                if let Some(sync) = app.try_state::<Arc<SyncController>>() {
                    let sync = Arc::clone(&sync);
                    std::thread::spawn(move || {
                        sync.stop();
                    });
                }
            } else if id == "quit" {
                app.exit(0);
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                if button == MouseButton::Left && button_state == MouseButtonState::Up {
                    show_and_focus_main(tray.app_handle());
                }
            }
        })
        .build(app)?;

    app.manage(tray);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let sync = Arc::new(SyncController::new());
    let sync_for_manage = sync.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(sync_for_manage)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            set_api_key_cmd,
            clear_api_key_cmd,
            test_connection,
            get_sync_status,
            get_storage_info,
            start_sync,
            stop_sync,
        ])
        .setup(move |app| {
            #[cfg(not(mobile))]
            build_tray(app.handle())?;

            #[cfg(not(mobile))]
            refresh_tray_icon(app.handle(), &sync);

            let cfg = load_config(app.handle()).unwrap_or_else(|_| AppConfig::default());
            let tray_flag = TrayMinimizeFlag::new(cfg.minimize_to_tray);
            app.manage(tray_flag.clone());

            #[cfg(not(mobile))]
            if let Some(window) = app.get_webview_window("main") {
                let app_h = app.handle().clone();
                let flag = tray_flag.clone();
                window.on_window_event(move |event| {
                    if !flag.get() {
                        return;
                    }
                    match event {
                        WindowEvent::CloseRequested { api, .. } => {
                            api.prevent_close();
                            if let Some(w) = app_h.get_webview_window("main") {
                                let _ = w.hide();
                            }
                        }
                        WindowEvent::Resized(_) => {
                            if let Some(w) = app_h.get_webview_window("main") {
                                if w.is_minimized().unwrap_or(false) {
                                    let _ = w.hide();
                                }
                            }
                        }
                        _ => {}
                    }
                });
            }

            if cfg.sync_enabled && get_api_key().ok().flatten().is_some() && !cfg.watch_paths.is_empty()
            {
                let h = app.handle().clone();
                let s = sync.clone();
                std::thread::spawn(move || {
                    let _ = s.start(&h, s.clone());
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
