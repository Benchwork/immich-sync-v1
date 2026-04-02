use crate::config::{db_path, is_supported_media, load_config, AppConfig};
use crate::credentials::get_api_key;
use crate::db::SyncDatabase;
use crate::immich::{file_sha1_hex, upload_asset};
use notify_debouncer_full::{
    new_debouncer, notify::RecommendedWatcher, notify::RecursiveMode, DebounceEventResult,
    Debouncer, DebouncedEvent, RecommendedCache,
};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::AppHandle;
use walkdir::WalkDir;

pub struct SyncMetrics {
    pub last_error: Mutex<Option<String>>,
    pub last_upload_ms: AtomicU64,
    pub uploads_ok: AtomicU64,
}

impl SyncMetrics {
    fn new() -> Self {
        Self {
            last_error: Mutex::new(None),
            last_upload_ms: AtomicU64::new(0),
            uploads_ok: AtomicU64::new(0),
        }
    }

    fn record_ok(&self) {
        self.uploads_ok.fetch_add(1, Ordering::SeqCst);
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_upload_ms.store(ms, Ordering::SeqCst);
        *self.last_error.lock() = None;
    }

    fn record_err(&self, msg: String) {
        *self.last_error.lock() = Some(msg);
    }
}

pub struct SyncController {
    inner: Mutex<Option<SyncRun>>,
    pub metrics: Arc<SyncMetrics>,
}

struct SyncRun {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    worker: std::thread::JoinHandle<()>,
}

impl SyncController {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            metrics: Arc::new(SyncMetrics::new()),
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().is_some()
    }

    pub fn status(&self) -> SyncStatusDto {
        let m = &self.metrics;
        SyncStatusDto {
            running: self.is_running(),
            last_error: m.last_error.lock().clone(),
            last_upload_ms: match m.last_upload_ms.load(Ordering::SeqCst) {
                0 => None,
                n => Some(n),
            },
            uploads_ok: m.uploads_ok.load(Ordering::SeqCst),
        }
    }

    pub fn stop(&self) {
        let mut guard = self.inner.lock();
        if let Some(run) = guard.take() {
            run.debouncer.stop();
            let _ = run.worker.join();
        }
    }

    /// Clears the last sync error (e.g. after saving a valid API key so stale config errors go away).
    pub fn clear_last_error(&self) {
        *self.metrics.last_error.lock() = None;
    }

    pub fn start(&self, app: &AppHandle) -> Result<(), String> {
        self.stop();
        let cfg = load_config(app)?;
        if !cfg.sync_enabled {
            return Err("Sync is disabled in settings".to_string());
        }
        if get_api_key()?.is_none() {
            return Err(
                "No API key saved for sync. Paste the key and click Save settings, or click Start sync while the key is still in the API key field."
                    .to_string(),
            );
        }
        if cfg.watch_paths.is_empty() {
            return Err("Add at least one folder to watch".to_string());
        }
        if cfg.server_url.trim().is_empty() {
            return Err("Server URL is required".to_string());
        }

        let app_clone = app.clone();
        let watch_paths: Vec<PathBuf> = cfg.watch_paths.iter().map(PathBuf::from).collect();
        let metrics = self.metrics.clone();

        let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();

        let worker = std::thread::spawn(move || {
            let db_path = match db_path(&app_clone) {
                Ok(p) => p,
                Err(e) => {
                    metrics.record_err(e);
                    return;
                }
            };
            let db = match SyncDatabase::open(&db_path) {
                Ok(d) => d,
                Err(e) => {
                    metrics.record_err(e);
                    return;
                }
            };

            for res in rx {
                match res {
                    Ok(events) => {
                        for e in events {
                            handle_debounced_event(&e, &app_clone, &db, &metrics);
                        }
                    }
                    Err(errs) => {
                        let msg = errs
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join("; ");
                        metrics.record_err(msg);
                    }
                }
            }
        });

        let mut debouncer = new_debouncer(std::time::Duration::from_secs(2), None, tx)
            .map_err(|e| e.to_string())?;

        for watch in &watch_paths {
            if watch.exists() {
                debouncer
                    .watch(watch, RecursiveMode::Recursive)
                    .map_err(|e| e.to_string())?;
            }
        }

        let initial_cfg = cfg.clone();
        let app_scan = app.clone();
        let scan_metrics = self.metrics.clone();
        std::thread::spawn(move || {
            initial_scan(&app_scan, &initial_cfg, &scan_metrics);
        });

        *self.inner.lock() = Some(SyncRun { debouncer, worker });
        Ok(())
    }
}

fn handle_debounced_event(
    e: &DebouncedEvent,
    app: &AppHandle,
    db: &SyncDatabase,
    metrics: &Arc<SyncMetrics>,
) {
    for path in &e.paths {
        if !path.is_file() || !is_supported_media(path) {
            continue;
        }
        let cfg = match load_config(app) {
            Ok(c) => c,
            Err(err) => {
                metrics.record_err(err);
                continue;
            }
        };
        if !cfg.sync_enabled {
            continue;
        }
        match process_file(path, &cfg, db) {
            Ok(UploadOutcome::Uploaded) => metrics.record_ok(),
            Ok(UploadOutcome::Skipped) => {}
            Err(err) => metrics.record_err(err),
        }
    }
}

#[derive(Clone, Copy)]
enum UploadOutcome {
    Skipped,
    Uploaded,
}

fn process_file(path: &Path, cfg: &AppConfig, db: &SyncDatabase) -> Result<UploadOutcome, String> {
    let api_key = get_api_key()?.ok_or("API key missing")?;
    let path_str = path.to_string_lossy().to_string();
    let sha1 = file_sha1_hex(path)?;
    if db
        .get_sha1(&path_str)
        .ok()
        .flatten()
        .as_deref()
        == Some(sha1.as_str())
    {
        return Ok(UploadOutcome::Skipped);
    }
    let res = upload_asset(&cfg.server_url, &api_key, path, &sha1)?;
    db.upsert(
        &path_str,
        &sha1,
        Some(&res.id),
        res.duplicate,
    )?;
    Ok(UploadOutcome::Uploaded)
}

fn initial_scan(app: &AppHandle, cfg: &AppConfig, metrics: &Arc<SyncMetrics>) {
    let Some(api_key) = (match get_api_key() {
        Ok(k) => k,
        Err(e) => {
            metrics.record_err(e);
            return;
        }
    }) else {
        return;
    };
    let db_path = match db_path(app) {
        Ok(p) => p,
        Err(e) => {
            metrics.record_err(e);
            return;
        }
    };
    let db = match SyncDatabase::open(&db_path) {
        Ok(d) => d,
        Err(e) => {
            metrics.record_err(e);
            return;
        }
    };
    for root in &cfg.watch_paths {
        let root_path = Path::new(root);
        if !root_path.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root_path).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() || !is_supported_media(path) {
                continue;
            }
            match process_file_with_key(path, cfg, &api_key, &db) {
                Ok(UploadOutcome::Uploaded) => metrics.record_ok(),
                Ok(UploadOutcome::Skipped) => {}
                Err(err) => metrics.record_err(err),
            }
        }
    }
}

fn process_file_with_key(
    path: &Path,
    cfg: &AppConfig,
    api_key: &str,
    db: &SyncDatabase,
) -> Result<UploadOutcome, String> {
    let path_str = path.to_string_lossy().to_string();
    let sha1 = file_sha1_hex(path)?;
    if db
        .get_sha1(&path_str)
        .ok()
        .flatten()
        .as_deref()
        == Some(sha1.as_str())
    {
        return Ok(UploadOutcome::Skipped);
    }
    let res = upload_asset(&cfg.server_url, api_key, path, &sha1)?;
    db.upsert(
        &path_str,
        &sha1,
        Some(&res.id),
        res.duplicate,
    )?;
    Ok(UploadOutcome::Uploaded)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusDto {
    pub running: bool,
    pub last_error: Option<String>,
    pub last_upload_ms: Option<u64>,
    pub uploads_ok: u64,
}
