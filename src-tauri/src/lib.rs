mod config;
mod credentials;
mod db;
mod immich;
mod sync;

use config::{load_config, save_config, AppConfig};
use credentials::{clear_api_key, get_api_key, set_api_key};
use immich::verify_connection;
use serde::{Deserialize, Serialize};
use sync::{SyncController, SyncStatusDto};
use std::sync::Arc;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDto {
    pub server_url: String,
    pub watch_paths: Vec<String>,
    pub sync_enabled: bool,
    pub has_api_key: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSettingsInput {
    pub server_url: String,
    pub watch_paths: Vec<String>,
    pub sync_enabled: bool,
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
        has_api_key: get_api_key().ok().flatten().is_some(),
    })
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    sync: tauri::State<'_, Arc<SyncController>>,
    input: SaveSettingsInput,
) -> Result<(), String> {
    let config = AppConfig {
        server_url: input.server_url,
        watch_paths: input.watch_paths,
        sync_enabled: input.sync_enabled,
    };
    save_config(&app, &config)?;
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
fn get_sync_status(sync: tauri::State<'_, Arc<SyncController>>) -> Result<SyncStatusDto, String> {
    Ok(sync.status())
}

#[tauri::command]
fn start_sync(
    app: tauri::AppHandle,
    sync: tauri::State<'_, Arc<SyncController>>,
) -> Result<(), String> {
    sync.start(&app)
}

#[tauri::command]
fn stop_sync(sync: tauri::State<'_, Arc<SyncController>>) -> Result<(), String> {
    sync.stop();
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
            start_sync,
            stop_sync,
        ])
        .setup(move |app| {
            let cfg = load_config(app.handle()).unwrap_or_else(|_| AppConfig::default());
            if cfg.sync_enabled && get_api_key().ok().flatten().is_some() && !cfg.watch_paths.is_empty()
            {
                let h = app.handle().clone();
                let s = sync.clone();
                std::thread::spawn(move || {
                    let _ = s.start(&h);
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
