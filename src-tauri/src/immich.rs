use crate::config::normalize_base_url;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use reqwest::blocking::multipart;
use reqwest::blocking::Client;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub duplicate: bool,
}

/// Whole-request timeout (connect + upload + read response). Large LAN uploads may need this high.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(3600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Ping / storage probes must fail fast when the host is down so the UI never freezes for a minute.
const QUICK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUICK_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const STORAGE_CACHE_TTL: Duration = Duration::from_secs(45);

/// Require at least this multiple of the upload size free on the Immich host (best-effort).
const SERVER_FREE_MULTIPLIER: u64 = 2;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStorage {
    #[serde(default)]
    pub disk_available: String,
    #[serde(default)]
    pub disk_available_raw: u64,
    #[serde(default)]
    pub disk_usage_percentage: f64,
}

static SERVER_STORAGE_CACHE: Mutex<Option<(String, Instant, Option<ServerStorage>)>> =
    Mutex::new(None);

fn build_client() -> Result<Client, String> {
    Client::builder()
        .timeout(UPLOAD_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn build_light_client() -> Result<Client, String> {
    Client::builder()
        .timeout(QUICK_REQUEST_TIMEOUT)
        .connect_timeout(QUICK_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn build_ping_client() -> Result<Client, String> {
    Client::builder()
        .timeout(QUICK_REQUEST_TIMEOUT)
        .connect_timeout(QUICK_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn build_version_probe_client() -> Result<Client, String> {
    Client::builder()
        .timeout(QUICK_REQUEST_TIMEOUT)
        .connect_timeout(QUICK_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

/// `GET /api/server/ping` — unauthenticated; confirms the Immich HTTP service is reachable.
pub fn check_server_online(base_url: &str) -> Result<(), String> {
    let base = normalize_base_url(base_url);
    let url = format!("{}/api/server/ping", base);
    let client = build_ping_client()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .map_err(|e| format!("Immich server unreachable: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "Immich server returned HTTP {} (not ready)",
            resp.status()
        ))
    }
}

/// Cached `GET /api/server/storage`. Returns `None` if the key lacks `server.storage` (403/401).
pub fn fetch_server_storage_cached(
    base_url: &str,
    api_key: &str,
) -> Result<Option<ServerStorage>, String> {
    let base = normalize_base_url(base_url);
    let now = Instant::now();
    {
        let guard = SERVER_STORAGE_CACHE.lock();
        if let Some((ref cached_base, at, ref data)) = *guard {
            if cached_base == &base && now.duration_since(at) < STORAGE_CACHE_TTL {
                return Ok(data.clone());
            }
        }
    }

    let url = format!("{}/api/server/storage", base);
    let client = build_light_client()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .header("x-api-key", api_key)
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
        *SERVER_STORAGE_CACHE.lock() = Some((base.clone(), now, None));
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!(
            "Server returned {}: {}",
            status,
            resp.text().unwrap_or_default()
        ));
    }
    let text = resp.text().map_err(|e| e.to_string())?;
    let body: ServerStorage = serde_json::from_str(&text)
        .map_err(|e| format!("Bad storage JSON: {} — {}", e, text))?;
    *SERVER_STORAGE_CACHE.lock() = Some((base, now, Some(body.clone())));
    Ok(Some(body))
}

/// Fails before upload when the Immich host is clearly too full (best-effort; 401/403 skips storage check).
pub fn precheck_upload_space(
    base_url: &str,
    api_key: &str,
    file_size: u64,
) -> Result<(), String> {
    match fetch_server_storage_cached(base_url, api_key) {
        Err(_) => Ok(()),
        Ok(None) => Ok(()),
        Ok(Some(s)) => {
            if s.disk_usage_percentage >= 99.0 {
                return Err(format!(
                    "Immich server disk is nearly full ({:.1}% used). Free space on the server before uploading.",
                    s.disk_usage_percentage
                ));
            }
            let need = file_size.saturating_mul(SERVER_FREE_MULTIPLIER);
            if s.disk_available_raw > 0 && s.disk_available_raw < need {
                return Err(format!(
                    "Immich server is low on free space ({} bytes free; this upload needs on the order of {} bytes on the server).",
                    s.disk_available_raw, need
                ));
            }
            Ok(())
        }
    }
}

fn format_reqwest_chain(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    let mut n = 0;
    while let Some(x) = src {
        if n < 6 {
            s.push_str(" → ");
            s.push_str(&x.to_string());
        }
        src = x.source();
        n += 1;
    }
    s
}

fn upload_io_err(path: &Path, context: &str, e: reqwest::Error) -> String {
    let path_str = path.display();
    let chain = format_reqwest_chain(&e);
    format!(
        "{context} ({path_str}): {chain}. \
         If this repeats on large files, check Immich / reverse-proxy timeouts (e.g. nginx proxy_read_timeout, client_max_body_size) and your network."
    )
}

pub fn verify_connection(base_url: &str, api_key: &str) -> Result<String, String> {
    let base = normalize_base_url(base_url);
    let url = format!("{}/api/server/version", base);
    let client = build_version_probe_client()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .header("x-api-key", api_key)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!(
            "Server returned {}: {}",
            resp.status(),
            resp.text().unwrap_or_default()
        ));
    }
    let body: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    if let Some(s) = body.get("version").and_then(|x| x.as_str()) {
        return Ok(s.to_string());
    }
    let major = body.get("major").and_then(|x| x.as_u64());
    let minor = body.get("minor").and_then(|x| x.as_u64());
    let patch = body.get("patch").and_then(|x| x.as_u64());
    let v = match (major, minor, patch) {
        (Some(a), Some(b), Some(c)) => format!("{}.{}.{}", a, b, c),
        _ => "unknown".to_string(),
    };
    Ok(v)
}

fn file_times(path: &Path) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let modified = meta.modified().map_err(|e| e.to_string())?;
    let created = meta.created().unwrap_or(modified);
    let modified_dt: DateTime<Utc> = modified.into();
    let created_dt: DateTime<Utc> = created.into();
    Ok((created_dt, modified_dt))
}

pub fn file_sha1_hex(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    let mut hasher = Sha1::new();
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn upload_asset(
    base_url: &str,
    api_key: &str,
    path: &Path,
    sha1_hex: &str,
) -> Result<UploadResponse, String> {
    let base = normalize_base_url(base_url);
    let url = format!("{}/api/assets", base);
    let (created, modified) = file_times(path)?;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let path_str = path.display().to_string();
    let device_asset_id = format!("{}-{}", path_str, mtime);

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload.bin");

    let part = multipart::Part::file(path)
        .map_err(|e| e.to_string())?
        .file_name(file_name.to_string());

    let form = multipart::Form::new()
        .text("deviceAssetId", device_asset_id)
        .text("deviceId", "immich-sync-windows")
        .text("fileCreatedAt", created.to_rfc3339())
        .text("fileModifiedAt", modified.to_rfc3339())
        .text("isFavorite", "false")
        .part("assetData", part);

    let client = build_client()?;
    let resp = client
        .post(&url)
        .header("Accept", "application/json")
        .header("x-api-key", api_key)
        .header("x-immich-checksum", sha1_hex)
        .multipart(form)
        .send()
        .map_err(|e| upload_io_err(path, "Upload request failed", e))?;

    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| upload_io_err(path, "Reading upload response failed", e))?;
    if !status.is_success() {
        return Err(format!("Upload failed ({}): {}", status, text));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad response JSON: {} — {}", e, text))
}
