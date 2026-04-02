use keyring::Entry;
use std::fs;
use std::path::PathBuf;

/// Must match `identifier` in `tauri.conf.json` and Tauri `app_data_dir()`.
const APP_DIR_NAME: &str = "com.pac1m.immich-sync";
const SERVICE: &str = "com.pac1m.immich-sync";
const USER_API_KEY: &str = "api_key";
/// Fallback when Windows Credential Manager is unavailable or inconsistent.
const API_KEY_FILE: &str = "api_key.secret";

fn api_key_file_path() -> Result<PathBuf, String> {
    let base = dirs::data_dir().ok_or_else(|| "Could not resolve app data directory".to_string())?;
    Ok(base.join(APP_DIR_NAME).join(API_KEY_FILE))
}

fn read_key_file() -> Result<Option<String>, String> {
    let path = api_key_file_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(s.to_string()))
}

fn write_key_file(key: &str) -> Result<(), String> {
    let path = api_key_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&path, key.as_bytes()).map_err(|e| e.to_string())
}

fn delete_key_file() {
    if let Ok(path) = api_key_file_path() {
        let _ = fs::remove_file(path);
    }
}

pub fn get_api_key() -> Result<Option<String>, String> {
    // Prefer on-disk secret (always written on save). Keyring alone can be stale if CredWrite failed.
    if let Some(k) = read_key_file()? {
        return Ok(Some(k));
    }
    if let Ok(entry) = Entry::new(SERVICE, USER_API_KEY) {
        match entry.get_password() {
            Ok(p) => return Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Ok(None),
        }
    } else {
        Ok(None)
    }
}

pub fn set_api_key(key: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE, USER_API_KEY).map_err(|e| e.to_string())?;
    let _ = entry.set_password(key);
    write_key_file(key)?;
    Ok(())
}

pub fn clear_api_key() -> Result<(), String> {
    let entry = Entry::new(SERVICE, USER_API_KEY).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) => {}
        Err(keyring::Error::NoEntry) => {}
        Err(_) => {}
    }
    delete_key_file();
    Ok(())
}
