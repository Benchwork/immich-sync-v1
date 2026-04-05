use crate::config::{db_path, is_supported_media, load_config, AppConfig};
use crate::credentials::get_api_key;
use crate::db::SyncDatabase;
use crate::immich::{
    check_server_online, file_sha1_hex, precheck_upload_space, upload_asset,
};
use notify_debouncer_full::{
    new_debouncer, notify::RecommendedWatcher, notify::RecursiveMode, DebounceEventResult,
    Debouncer, DebouncedEvent, RecommendedCache,
};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::image::Image;
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Manager};
use walkdir::WalkDir;

/// Supported media files under configured watch folders (recursive).
pub fn count_local_supported_files(cfg: &AppConfig) -> u64 {
    let mut n = 0u64;
    for root in &cfg.watch_paths {
        let root_path = Path::new(root);
        if !root_path.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root_path).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() && is_supported_media(path) {
                n += 1;
            }
        }
    }
    n
}

pub struct SyncMetrics {
    pub last_error: Mutex<Option<String>>,
    pub last_upload_ms: AtomicU64,
    /// Full path of the file currently being hashed or uploaded (cleared when idle).
    pub current_file: Mutex<Option<String>>,
}

impl SyncMetrics {
    fn new() -> Self {
        Self {
            last_error: Mutex::new(None),
            last_upload_ms: AtomicU64::new(0),
            current_file: Mutex::new(None),
        }
    }

    fn set_current_file(&self, path: Option<String>) {
        *self.current_file.lock() = path;
    }

    fn clear_current_file(&self) {
        *self.current_file.lock() = None;
    }

    fn record_ok(&self) {
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

struct CurrentFileGuard<'a> {
    metrics: &'a SyncMetrics,
}

impl Drop for CurrentFileGuard<'_> {
    fn drop(&mut self) {
        self.metrics.clear_current_file();
    }
}

const LIBRARY_STATS_CACHE_TTL: Duration = Duration::from_secs(5);

pub struct SyncController {
    inner: Mutex<Option<SyncRun>>,
    pub metrics: Arc<SyncMetrics>,
    library_stats_cache: Mutex<Option<(Instant, u64, u64)>>,
    /// Last app handle from a successful `start` path — used to refresh tray when sync stops (e.g. offline).
    last_app_for_tray: Mutex<Option<AppHandle>>,
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
            library_stats_cache: Mutex::new(None),
            last_app_for_tray: Mutex::new(None),
        }
    }

    pub fn invalidate_library_cache(&self) {
        *self.library_stats_cache.lock() = None;
    }

    fn library_stats(&self, app: &AppHandle) -> Result<(u64, u64), String> {
        let now = Instant::now();
        {
            let g = self.library_stats_cache.lock();
            if let Some((t, total, uploaded)) = *g {
                if now.duration_since(t) < LIBRARY_STATS_CACHE_TTL {
                    return Ok((total, uploaded));
                }
            }
        }
        let cfg = load_config(app)?;
        let total = count_local_supported_files(&cfg);
        let uploaded = match db_path(app) {
            Ok(p) if p.exists() => SyncDatabase::open(&p)
                .and_then(|db| db.count_synced_files_existing_under_watch(&cfg.watch_paths))
                .unwrap_or(0),
            _ => 0,
        };
        *self.library_stats_cache.lock() = Some((now, total, uploaded));
        Ok((total, uploaded))
    }

    pub fn status_with_library(&self, app: &AppHandle) -> Result<SyncStatusDto, String> {
        let m = &self.metrics;
        let (local_files_total, local_files_uploaded) = self.library_stats(app)?;
        Ok(SyncStatusDto {
            running: self.is_running(),
            last_error: m.last_error.lock().clone(),
            last_upload_ms: match m.last_upload_ms.load(Ordering::SeqCst) {
                0 => None,
                n => Some(n),
            },
            local_files_total,
            local_files_uploaded,
            current_file: m.current_file.lock().clone(),
        })
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().is_some()
    }

    pub fn stop(&self) {
        let mut guard = self.inner.lock();
        if let Some(run) = guard.take() {
            run.debouncer.stop();
            let _ = run.worker.join();
        }
        self.metrics.clear_current_file();
        let app = self.last_app_for_tray.lock().clone();
        if let Some(ref h) = app {
            refresh_tray_icon(h, self);
        }
    }

    /// Clears the last sync error (e.g. after saving a valid API key so stale config errors go away).
    pub fn clear_last_error(&self) {
        *self.metrics.last_error.lock() = None;
    }

    pub fn start(&self, app: &AppHandle, controller: Arc<SyncController>) -> Result<(), String> {
        self.stop();
        *self.last_app_for_tray.lock() = Some(app.clone());
        let result = self.start_inner(app, controller);
        refresh_tray_icon(app, self);
        result
    }

    fn start_inner(&self, app: &AppHandle, controller: Arc<SyncController>) -> Result<(), String> {
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
        check_server_online(&cfg.server_url)?;

        let app_clone = app.clone();
        let controller_for_worker = controller.clone();
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
                            handle_debounced_event(
                                &e,
                                &app_clone,
                                &db,
                                &metrics,
                                &controller_for_worker,
                            );
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
        let controller_for_scan = controller.clone();
        std::thread::spawn(move || {
            initial_scan(
                &app_scan,
                &initial_cfg,
                &scan_metrics,
                &controller_for_scan,
            );
        });

        *self.inner.lock() = Some(SyncRun { debouncer, worker });
        self.invalidate_library_cache();
        Ok(())
    }
}

fn tray_icon_for_sync_running(running: bool) -> Result<Image<'static>, String> {
    const BYTES: &[u8] = include_bytes!("../icons/32x32.png");
    if running {
        return Image::from_bytes(BYTES).map_err(|e| e.to_string());
    }
    let img = image::load_from_memory(BYTES).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let p = rgba.get_pixel(x, y);
            let l = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8;
            data.extend_from_slice(&[l, l, l, p[3]]);
        }
    }
    Ok(Image::new_owned(data, w, h))
}

fn apply_tray_sync_state(tray: &TrayIcon, running: bool) -> Result<(), String> {
    let icon = tray_icon_for_sync_running(running)?;
    tray
        .set_icon(Some(icon))
        .map_err(|e| e.to_string())?;
    if running {
        tray
            .set_tooltip(Some("Immich Sync — sync running"))
            .map_err(|e| e.to_string())?;
    } else {
        tray
            .set_tooltip(Some("Immich Sync — sync stopped"))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn refresh_tray_icon(app: &AppHandle, sync: &SyncController) {
    let running = sync.is_running();
    let app = app.clone();
    let app_for_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = app_for_main.try_state::<TrayIcon>() {
            let _ = apply_tray_sync_state(&*tray, running);
        }
    });
}

fn handle_debounced_event(
    e: &DebouncedEvent,
    app: &AppHandle,
    db: &SyncDatabase,
    metrics: &Arc<SyncMetrics>,
    controller: &Arc<SyncController>,
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
        match process_file(path, &cfg, db, metrics) {
            Ok(UploadOutcome::Uploaded) => {
                metrics.record_ok();
                controller.invalidate_library_cache();
            }
            Ok(UploadOutcome::Skipped) => {}
            Err(ProcessErr::ServerOffline(msg)) => {
                metrics.record_err(format!(
                    "Immich server is offline or unreachable. Sync stopped. {msg}"
                ));
                let c = Arc::clone(controller);
                std::thread::spawn(move || {
                    c.stop();
                });
            }
            Err(ProcessErr::Other(err)) => metrics.record_err(err),
        }
    }
}

#[derive(Clone, Copy)]
enum UploadOutcome {
    Skipped,
    Uploaded,
}

enum ProcessErr {
    ServerOffline(String),
    Other(String),
}

fn process_file(
    path: &Path,
    cfg: &AppConfig,
    db: &SyncDatabase,
    metrics: &Arc<SyncMetrics>,
) -> Result<UploadOutcome, ProcessErr> {
    let api_key = get_api_key()
        .map_err(ProcessErr::Other)?
        .ok_or_else(|| ProcessErr::Other("API key missing".to_string()))?;
    let path_str = path.to_string_lossy().to_string();
    metrics.set_current_file(Some(path_str.clone()));
    let _current_guard = CurrentFileGuard {
        metrics: metrics.as_ref(),
    };
    let sha1 = file_sha1_hex(path).map_err(ProcessErr::Other)?;
    if db
        .get_sha1(&path_str)
        .ok()
        .flatten()
        .as_deref()
        == Some(sha1.as_str())
    {
        return Ok(UploadOutcome::Skipped);
    }
    check_server_online(&cfg.server_url).map_err(|e| ProcessErr::ServerOffline(e))?;
    let file_size = std::fs::metadata(path)
        .map_err(|e| ProcessErr::Other(e.to_string()))?
        .len();
    precheck_upload_space(&cfg.server_url, &api_key, file_size).map_err(ProcessErr::Other)?;
    let res = upload_asset(&cfg.server_url, &api_key, path, &sha1).map_err(ProcessErr::Other)?;
    db.upsert(
        &path_str,
        &sha1,
        Some(&res.id),
        res.duplicate,
    )
    .map_err(ProcessErr::Other)?;
    Ok(UploadOutcome::Uploaded)
}

fn initial_scan(
    app: &AppHandle,
    cfg: &AppConfig,
    metrics: &Arc<SyncMetrics>,
    controller: &Arc<SyncController>,
) {
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
            match process_file_with_key(path, cfg, &api_key, &db, metrics) {
                Ok(UploadOutcome::Uploaded) => {
                    metrics.record_ok();
                    controller.invalidate_library_cache();
                }
                Ok(UploadOutcome::Skipped) => {}
                Err(ProcessErr::ServerOffline(msg)) => {
                    metrics.record_err(format!(
                        "Immich server is offline or unreachable. Sync stopped. {msg}"
                    ));
                    let c = Arc::clone(controller);
                    std::thread::spawn(move || {
                        c.stop();
                    });
                    return;
                }
                Err(ProcessErr::Other(err)) => metrics.record_err(err),
            }
        }
    }
}

fn process_file_with_key(
    path: &Path,
    cfg: &AppConfig,
    api_key: &str,
    db: &SyncDatabase,
    metrics: &Arc<SyncMetrics>,
) -> Result<UploadOutcome, ProcessErr> {
    let path_str = path.to_string_lossy().to_string();
    metrics.set_current_file(Some(path_str.clone()));
    let _current_guard = CurrentFileGuard {
        metrics: metrics.as_ref(),
    };
    let sha1 = file_sha1_hex(path).map_err(ProcessErr::Other)?;
    if db
        .get_sha1(&path_str)
        .ok()
        .flatten()
        .as_deref()
        == Some(sha1.as_str())
    {
        return Ok(UploadOutcome::Skipped);
    }
    check_server_online(&cfg.server_url).map_err(|e| ProcessErr::ServerOffline(e))?;
    let file_size = std::fs::metadata(path)
        .map_err(|e| ProcessErr::Other(e.to_string()))?
        .len();
    precheck_upload_space(&cfg.server_url, api_key, file_size).map_err(ProcessErr::Other)?;
    let res = upload_asset(&cfg.server_url, api_key, path, &sha1).map_err(ProcessErr::Other)?;
    db.upsert(
        &path_str,
        &sha1,
        Some(&res.id),
        res.duplicate,
    )
    .map_err(ProcessErr::Other)?;
    Ok(UploadOutcome::Uploaded)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusDto {
    pub running: bool,
    pub last_error: Option<String>,
    pub last_upload_ms: Option<u64>,
    /// Supported media files found under watch folders.
    pub local_files_total: u64,
    /// Of those, how many still exist on disk and are recorded as synced in the local DB.
    pub local_files_uploaded: u64,
    pub current_file: Option<String>,
}
