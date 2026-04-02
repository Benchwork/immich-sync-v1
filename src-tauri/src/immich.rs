use crate::config::normalize_base_url;
use chrono::{DateTime, Utc};
use reqwest::blocking::multipart;
use reqwest::blocking::Client;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub duplicate: bool,
}

fn build_client() -> Result<Client, String> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())
}

pub fn verify_connection(base_url: &str, api_key: &str) -> Result<String, String> {
    let base = normalize_base_url(base_url);
    let url = format!("{}/api/server/version", base);
    let client = build_client()?;
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
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    let text = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Upload failed ({}): {}", status, text));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad response JSON: {} — {}", e, text))
}
