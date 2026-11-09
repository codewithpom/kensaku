use crate::AppState;
use crate::config::KensakuConfig;
use crate::indexer::IndexingStatus;
use crate::indexer::extract::image::{self, OcrStatus};
use crate::search::{SearchDispatcher, SearchResponse, SearchResult};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::str::FromStr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

static CURRENT_SEARCH_SEQ: AtomicU64 = AtomicU64::new(0);

const QUERY_LATENCY_CAPACITY: usize = 256;
const QUERY_HISTORY_SHOWN: usize = 50;

/// Process-global ring buffer of recent query wall times, in microseconds.
static QUERY_LATENCIES: OnceLock<Mutex<VecDeque<u64>>> = OnceLock::new();
static QUERY_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Process start time, used for the runtime uptime stat.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Records the process start time. Called once from `run()`.
pub fn init_process_start() {
    PROCESS_START.get_or_init(Instant::now);
}

fn record_query_latency(micros: u64) {
    QUERY_TOTAL.fetch_add(1, Ordering::Relaxed);
    let buffer =
        QUERY_LATENCIES.get_or_init(|| Mutex::new(VecDeque::with_capacity(QUERY_LATENCY_CAPACITY)));
    // Recover from a poisoned lock instead of panicking: stats are best-effort.
    let mut guard = match buffer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.len() >= QUERY_LATENCY_CAPACITY {
        guard.pop_front();
    }
    guard.push_back(micros);
}

fn query_latency_samples() -> Vec<u64> {
    let buffer =
        QUERY_LATENCIES.get_or_init(|| Mutex::new(VecDeque::with_capacity(QUERY_LATENCY_CAPACITY)));
    let guard = match buffer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.iter().copied().collect()
}

/// Nearest-rank percentile over ascending-sorted microsecond samples.
fn percentile_ms(sorted_micros: &[u64], percentile: f64) -> f64 {
    if sorted_micros.is_empty() {
        return 0.0;
    }
    let rank = ((percentile / 100.0) * sorted_micros.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted_micros.len() - 1);
    sorted_micros[index] as f64 / 1000.0
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Recursively sums regular files below `dir`. Symlinks are not followed.
fn dir_size_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total = total.saturating_add(dir_size_bytes(&entry.path()));
        } else if file_type.is_file() {
            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
    total
}

/// Sums sizes of entries in the kensaku config directory whose name starts with
/// `prefix` (e.g. `metadata.db` matches `metadata.db`, `metadata.db-wal`, ...).
fn config_prefixed_bytes(prefix: &str, include_dirs: bool) -> u64 {
    let dir = KensakuConfig::config_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(prefix) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if include_dirs {
                total = total.saturating_add(dir_size_bytes(&entry.path()));
            }
        } else if file_type.is_file() {
            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
    total
}

#[cfg(target_os = "linux")]
fn resident_set_bytes() -> u64 {
    // /proc/self/statm: field 1 (index 1) is resident set size in pages.
    const PAGE_SIZE: u64 = 4096;
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse::<u64>().ok())
        .map(|pages| pages.saturating_mul(PAGE_SIZE))
        .unwrap_or(0)
}

#[cfg(windows)]
fn resident_set_bytes() -> u64 {
    // Working set: this process's pages currently resident in physical
    // memory — the same number Task Manager shows. The hide-path's
    // EmptyWorkingSet trim legitimately drives it near zero while the
    // window is gone; a zero reading there is the trim working, not a bug.
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut pmc = PROCESS_MEMORY_COUNTERS::default();
    let read = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if read.is_ok() {
        pmc.WorkingSetSize as u64
    } else {
        0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn resident_set_bytes() -> u64 {
    0
}

#[tauri::command]
pub async fn search(
    query: String,
    seq: Option<u64>,
    sort: Option<String>,
    offset: Option<usize>,
    state: State<'_, AppState>,
) -> Result<SearchResponse, String> {
    let started = Instant::now();
    let req_seq = seq.unwrap_or_else(|| CURRENT_SEARCH_SEQ.fetch_add(1, Ordering::SeqCst) + 1);
    CURRENT_SEARCH_SEQ.store(req_seq, Ordering::SeqCst);

    if let Some(daemon) = state.daemon.as_ref() {
        let mut resp = daemon.search_page(
            query.clone(),
            state.indexer.config.read().max_results,
            offset.unwrap_or(0),
            false,
        )?;
        resp.seq = req_seq;
        record_query_latency(started.elapsed().as_micros() as u64);
        return Ok(resp);
    }

    let indexer = &state.indexer;
    indexer.query_in_flight.store(true, Ordering::SeqCst);

    let metadata = indexer.metadata.read();
    let content_index = indexer.content_index.read();
    let unit_store = indexer.unit_store.read();
    let cfg = indexer.config.read();
    let max_results = cfg.max_results;
    let semantic_enabled = cfg.semantic_enabled;
    let aliases = cfg.alias_pairs();

    // The UI asks for snippets separately (`get_snippets`) so the search
    // itself never touches file contents.
    let sort = sort
        .as_deref()
        .map(crate::search::SortOrder::parse)
        .unwrap_or_default();
    let visual_store = indexer.visual_store.read();
    let resp = SearchDispatcher::search_response_fast_sorted_with_snippets_and_visual(
        &query,
        &metadata,
        &content_index,
        Some(&unit_store),
        Some(&visual_store),
        semantic_enabled,
        max_results,
        req_seq,
        sort,
        &aliases,
        offset.unwrap_or(0),
        false,
    );

    indexer.query_in_flight.store(false, Ordering::SeqCst);
    record_query_latency(started.elapsed().as_micros() as u64);
    Ok(resp)
}

/// Snippet payload fetched in a second round-trip after search results render.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetDto {
    pub path: String,
    pub snippet: Option<String>,
}

/// Longest snippet returned to the frontend.
const MAX_SNIPPET_CHARS: usize = 499;
/// Upper bound on paths accepted by a single `get_snippets` call.
const MAX_SNIPPET_PATHS: usize = 50;

/// Computes snippets for up to 50 paths in parallel.
///
/// Deliberately extraction-free: plain text is read directly and other
/// formats are served from the extraction cache, so a preview request can
/// never trigger OCR or PDF parsing.
#[tauri::command]
pub async fn get_snippets(
    paths: Vec<String>,
    query: String,
    state: State<'_, AppState>,
) -> Result<Vec<SnippetDto>, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.get_snippets(paths, &query);
    }
    // File reads stay off the async runtime even though they are cheap.
    tauri::async_runtime::spawn_blocking(move || compute_snippets(paths, &query))
        .await
        .map_err(|e| format!("Snippet task failed: {}", e))
}

pub fn compute_snippets(paths: Vec<String>, query: &str) -> Vec<SnippetDto> {
    use rayon::prelude::*;

    let paths: Vec<String> = paths.into_iter().take(MAX_SNIPPET_PATHS).collect();
    paths
        .par_iter()
        .map(|path| {
            let snippet = crate::indexer::content::snippet_for_path(Path::new(path), query)
                .map(|snippet| crate::agent::truncate_chars(&snippet, MAX_SNIPPET_CHARS));
            SnippetDto {
                path: path.clone(),
                snippet,
            }
        })
        .collect()
}

#[tauri::command]
pub async fn legacy_search(
    query: String,
    state: State<'_, AppState>,
) -> Result<Vec<SearchResult>, String> {
    let started = Instant::now();
    if let Some(daemon) = state.daemon.as_ref() {
        let resp = daemon.search_page(
            query.clone(),
            state.indexer.config.read().max_results,
            0,
            false,
        )?;
        record_query_latency(started.elapsed().as_micros() as u64);
        return Ok(resp.results);
    }

    let indexer = &state.indexer;
    indexer.query_in_flight.store(true, Ordering::SeqCst);

    let metadata = indexer.metadata.read();
    let content_index = indexer.content_index.read();
    let unit_store = indexer.unit_store.read();
    let cfg = indexer.config.read();
    let max_results = cfg.max_results;
    let semantic_enabled = cfg.semantic_enabled;

    let results = SearchDispatcher::search(
        &query,
        &metadata,
        &content_index,
        Some(&unit_store),
        semantic_enabled,
        max_results,
    );

    indexer.query_in_flight.store(false, Ordering::SeqCst);
    record_query_latency(started.elapsed().as_micros() as u64);
    Ok(results)
}

#[tauri::command]
pub async fn get_indexing_status(state: State<'_, AppState>) -> Result<IndexingStatus, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.get_status();
    }
    Ok(state.indexer.get_status())
}

pub fn stats_for(indexer: &crate::indexer::IndexerManager) -> Result<serde_json::Value, String> {
    let status = indexer.get_status();

    let (files, dirs) = {
        let metadata = indexer.metadata.read();
        let mut files = 0u64;
        let mut dirs = 0u64;
        for i in 0..metadata.len() {
            if metadata.node(i as u32).is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
        }
        (files, dirs)
    };

    let semantic_units = indexer.unit_store.read().len() as u64;
    // Real Tantivy document count, independent of the current session's
    // `content_indexed` counter (which resets on every launch).
    let content_docs = indexer
        .content_index
        .read()
        .as_ref()
        .map(|index| index.num_docs())
        .unwrap_or(0);

    let metadata_db_bytes = config_prefixed_bytes("metadata.db", false);
    let tantivy_bytes = dir_size_bytes(&KensakuConfig::index_dir());
    let vector_bytes = {
        let db = indexer.metadata.read().db();
        db.map(|db| db.vector_bytes().unwrap_or(0)).unwrap_or(0)
    };

    let embed_queue_depth = {
        let db = indexer.metadata.read().db();
        db.map(|db| db.count_unembedded().unwrap_or(0)).unwrap_or(0)
    };

    let now_ms = now_unix_ms();
    let now_secs = now_ms / 1000;

    let last_reconcile_secs_ago: Option<u64> = {
        let last = indexer.last_reconcile_unix_secs.load(Ordering::Relaxed);
        if last == 0 {
            None
        } else {
            Some(now_secs.saturating_sub(last))
        }
    };

    let files_per_sec = if status.is_indexing {
        let started_ms = indexer.crawl_start_unix_ms.load(Ordering::Relaxed);
        let elapsed_secs = if started_ms > 0 && now_ms > started_ms {
            (now_ms - started_ms) as f64 / 1000.0
        } else {
            0.0
        };
        if elapsed_secs > 0.0 {
            status.files_indexed as f64 / elapsed_secs
        } else {
            0.0
        }
    } else {
        0.0
    };

    let mut samples = query_latency_samples();
    let total_queries = QUERY_TOTAL.load(Ordering::Relaxed);
    let history: Vec<f64> = samples
        .iter()
        .skip(samples.len().saturating_sub(QUERY_HISTORY_SHOWN))
        .map(|micros| *micros as f64 / 1000.0)
        .collect();
    let last_ms = samples
        .last()
        .map(|micros| *micros as f64 / 1000.0)
        .unwrap_or(0.0);

    samples.sort_unstable();
    let p50_ms = percentile_ms(&samples, 50.0);
    let p95_ms = percentile_ms(&samples, 95.0);
    let p99_ms = percentile_ms(&samples, 99.0);

    let uptime_secs = PROCESS_START.get_or_init(Instant::now).elapsed().as_secs();
    let rss_bytes = resident_set_bytes();
    let watcher_ok = indexer.watcher_ok.load(Ordering::Relaxed);
    let model_id = indexer.config.read().semantic_model.clone();

    Ok(serde_json::json!({
        "queries": {
            "total": total_queries,
            "p50_ms": p50_ms,
            "p95_ms": p95_ms,
            "p99_ms": p99_ms,
            "last_ms": last_ms,
            "history": history,
        },
        "index": {
            "files": files,
            "dirs": dirs,
            "content_docs": content_docs,
            "content_rebuilt": indexer.content_index.read().as_ref().map(|i| i.rebuilt).unwrap_or(false),
            "rebuild_reason": indexer.content_index.read().as_ref().and_then(|i| i.rebuild_reason.clone()),
            "semantic_units": semantic_units,
            "metadata_db_bytes": metadata_db_bytes,
            "tantivy_bytes": tantivy_bytes,
            "vector_bytes": vector_bytes,
            "last_reconcile_secs_ago": last_reconcile_secs_ago,
        },
        "ingestion": {
            "is_indexing": status.is_indexing,
            "phase": status.phase.clone(),
            "files_indexed": status.files_indexed as u64,
            "content_indexed": status.content_indexed as u64,
            "units_indexed": status.units_indexed as u64,
            "visual_units": indexer.visual_store.read().len() as u64,
            "visual_available": crate::embed::visual::visual_available(),
            "files_per_sec": files_per_sec,
            "eta_secs": Option::<u64>::None,
            "embed_queue_depth": embed_queue_depth as u64,
        },
        "runtime": {
            "uptime_secs": uptime_secs,
            "rss_bytes": rss_bytes,
            "watcher_ok": watcher_ok,
            "semantic_ready": status.semantic_ready,
            "model_id": model_id,
            "semantic_provider": crate::embed::ModelRegistry::global().provider_name(),
            "semantic_dim": crate::embed::ModelRegistry::global().dim,
            "semantic_fallback_reason": crate::embed::ModelRegistry::global().fallback_reason.clone(),
        },
    }))
}

#[tauri::command]
pub async fn get_stats(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.get_stats();
    }
    stats_for(&state.indexer)
}

#[tauri::command]
pub async fn set_theme(
    app: AppHandle,
    theme: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lowered = theme.trim().to_ascii_lowercase();
    // "paper" / "void" are the pre-1.1 names for the same two themes.
    let normalized = match lowered.as_str() {
        "light" | "paper" => "light",
        "dark" | "void" => "dark",
        _ => {
            return Err(format!(
                "Unsupported theme '{}'. Expected 'light' or 'dark'.",
                theme
            ));
        }
    }
    .to_string();

    let mut config = state.indexer.config.read().clone();
    config.theme = normalized.clone();
    config.save()?;
    *state.indexer.config.write() = config;

    // On Windows, swap the tray icon to match the new theme so it remains
    // visible on both light and dark taskbars.  A tray failure is logged but
    // never returned as an error: a failed icon swap must never block saving
    // the theme.
    #[cfg(windows)]
    {
        if let Some(tray) = app.tray_by_id("main") {
            let icon_bytes = crate::tray_icon::tray_icon_bytes(&normalized);
            match crate::tray_icon::decode_png(icon_bytes)
                .or_else(|| app.default_window_icon().cloned())
            {
                Some(icon) => {
                    if let Err(e) = tray.set_icon(Some(icon)) {
                        eprintln!("[Kensaku] tray icon update failed: {e}");
                    }
                }
                None => {
                    eprintln!("[Kensaku] tray icon: could not decode PNG for theme '{normalized}'")
                }
            }
        }
    }
    // Suppress unused-variable warning on non-Windows targets.
    #[cfg(not(windows))]
    let _ = app;

    Ok(())
}

#[tauri::command]
pub async fn set_surface(surface: String, state: State<'_, AppState>) -> Result<(), String> {
    let normalized = crate::config::parse_surface(&surface)
        .ok_or_else(|| format!("Unsupported surface '{surface}'. Expected 'solid', 'frost' or 'glass'."))?;
    let mut config = state.indexer.config.read().clone();
    config.surface = normalized.to_string();
    config.save()?;
    *state.indexer.config.write() = config;
    Ok(())
}

#[tauri::command]
pub async fn open_file(path: String, state: State<'_, AppState>) -> Result<(), String> {
    state.indexer.metadata.read().record_open(Path::new(&path));
    open::that(&path).map_err(|e| format!("Failed to open file: {}", e))
}

#[tauri::command]
pub async fn open_folder(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    let target_dir = if p.is_dir() {
        p
    } else {
        p.parent().unwrap_or(p)
    };

    open::that(target_dir).map_err(|e| format!("Failed to open folder: {}", e))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePreview {
    pub path: String,
    pub filename: String,
    pub file_size: u64,
    pub modified: u64,
    pub is_dir: bool,
    pub preview_type: String,
    pub content: Option<String>,
    pub total_lines: Option<usize>,
    pub child_count: Option<usize>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[tauri::command]
pub async fn get_file_preview(path: String) -> Result<FilePreview, String> {
    // Metadata, image decoding and document extraction are all blocking work;
    // keep them off the async runtime.
    tauri::async_runtime::spawn_blocking(move || build_file_preview(&path))
        .await
        .map_err(|e| format!("Preview task failed: {}", e))?
}

fn build_file_preview(path: &str) -> Result<FilePreview, String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err("File does not exist".to_string());
    }

    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    let filename = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let file_size = meta.len();
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let is_dir = meta.is_dir();

    if is_dir {
        let child_count = std::fs::read_dir(p).map(|rd| rd.count()).unwrap_or(0);
        return Ok(FilePreview {
            path: path.to_string(),
            filename,
            file_size,
            modified,
            is_dir: true,
            preview_type: "dir".to_string(),
            content: None,
            total_lines: None,
            child_count: Some(child_count),
            width: None,
            height: None,
        });
    }

    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Raster images are previewed by the webview directly. SVG/ICO are handled
    // the same way (the browser decodes them), but they have no raster
    // dimensions here. This check runs before any extraction so an image can
    // never reach tesseract on preview.
    let is_image = crate::indexer::extract::image::is_supported_image_extension(&ext)
        || matches!(ext.as_str(), "svg" | "ico");
    if is_image {
        let dimensions = crate::indexer::extract::image::dimensions(p);
        return Ok(FilePreview {
            path: path.to_string(),
            filename,
            file_size,
            modified,
            is_dir: false,
            preview_type: "image".to_string(),
            content: None,
            total_lines: None,
            child_count: None,
            width: dimensions.map(|(width, _)| width),
            height: dimensions.map(|(_, height)| height),
        });
    }

    if let Some(doc) = crate::indexer::extract::extract_document(p, 128_000) {
        let preview_type = match crate::indexer::extract::sniff_format(p) {
            crate::indexer::extract::DocumentFormat::Pdf => "pdf",
            crate::indexer::extract::DocumentFormat::Docx => "doc",
            crate::indexer::extract::DocumentFormat::Pptx => "presentation",
            crate::indexer::extract::DocumentFormat::Xlsx => "sheet",
            crate::indexer::extract::DocumentFormat::Epub
            | crate::indexer::extract::DocumentFormat::Odt
            | crate::indexer::extract::DocumentFormat::Rtf => "doc",
            _ => "text",
        };

        let total_lines = doc.text.lines().count();
        return Ok(FilePreview {
            path: path.to_string(),
            filename,
            file_size,
            modified,
            is_dir: false,
            preview_type: preview_type.to_string(),
            content: Some(doc.text),
            total_lines: Some(total_lines),
            child_count: doc.page_count,
            width: None,
            height: None,
        });
    }

    Ok(FilePreview {
        path: path.to_string(),
        filename,
        file_size,
        modified,
        is_dir: false,
        preview_type: "binary".to_string(),
        content: None,
        total_lines: None,
        child_count: None,
        width: None,
        height: None,
    })
}

/// Cache directory for generated image thumbnails.
fn thumbnail_cache_dir() -> PathBuf {
    KensakuConfig::config_dir().join("thumbs")
}

/// Creates the thumbnail cache directory with owner-only permissions.
fn ensure_thumbnail_cache_dir() -> Result<PathBuf, String> {
    let dir = thumbnail_cache_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create thumbnail cache directory: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Failed to secure thumbnail cache directory: {}", e))?;
    }
    Ok(dir)
}

fn generate_thumbnail(path: &str) -> Result<String, String> {
    let source = Path::new(path);
    let dir = ensure_thumbnail_cache_dir()?;
    let preview = crate::preview::preview_jpeg(source, &dir)?;
    preview
        .to_str()
        .map(|p| p.to_string())
        .ok_or_else(|| "Preview path is not valid UTF-8".to_string())
}

/// Returns an absolute path to a cached 320px JPEG thumbnail.
///
/// The frontend turns this path into an asset URL with `convertFileSrc`. Only
/// raster formats supported by the decoder are accepted; anything else returns
/// `"not an image"`.
#[tauri::command]
pub async fn get_thumbnail(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || generate_thumbnail(&path))
        .await
        .map_err(|e| format!("Thumbnail task failed: {}", e))?
}

/// Reports whether tesseract and its language data are usable.
///
/// The engine is probed by spawning `tesseract --version`, so the work runs on
/// a blocking thread to keep the async runtime responsive.
#[tauri::command]
pub async fn get_ocr_status() -> Result<OcrStatus, String> {
    tauri::async_runtime::spawn_blocking(image::ocr_status)
        .await
        .map_err(|e| format!("OCR status task failed: {}", e))
}

/// Downloads the English tessdata file, then returns the refreshed status.
#[tauri::command]
pub async fn download_ocr_language() -> Result<OcrStatus, String> {
    tauri::async_runtime::spawn_blocking(|| {
        image::download_eng_tessdata()?;
        Ok::<OcrStatus, String>(image::ocr_status())
    })
    .await
    .map_err(|e| format!("OCR download task failed: {}", e))?
}

/// Reports the optional accurate OCR pack inventory.
#[tauri::command]
pub async fn get_ocr_pack_status() -> Result<crate::models::ModelsStatus, String> {
    Ok(crate::models::ocr_status())
}

/// Starts the background download of the accurate OCR pack.
#[tauri::command]
pub async fn download_ocr_pack() -> Result<crate::models::ModelsStatus, String> {
    crate::models::download_ocr_pack()?;
    Ok(crate::models::ocr_status())
}

/// Reports the optional Indra OCR pack inventory.
#[tauri::command]
pub async fn get_indra_pack_status() -> Result<crate::models::ModelsStatus, String> {
    Ok(crate::models::indra_status())
}

/// Starts the background download of the Indra OCR pack.
#[tauri::command]
pub async fn download_indra_pack() -> Result<crate::models::ModelsStatus, String> {
    crate::models::download_indra_pack()?;
    Ok(crate::models::indra_status())
}

/// Reports the optional smart embedding pack inventory.
#[tauri::command]
pub async fn get_smart_pack_status() -> Result<crate::models::ModelsStatus, String> {
    Ok(crate::models::smart_status())
}

/// Starts the background download of the smart embedding pack.
#[tauri::command]
pub async fn download_smart_pack() -> Result<crate::models::ModelsStatus, String> {
    crate::models::download_smart_pack()?;
    Ok(crate::models::smart_status())
}

/// Reports the optional beta visual-search model pack inventory.
#[tauri::command]
pub async fn get_visual_pack_status() -> Result<crate::models::ModelsStatus, String> {
    Ok(crate::models::visual_status())
}

/// Starts the background download of the beta visual-search model pack.
#[tauri::command]
pub async fn download_visual_pack() -> Result<crate::models::ModelsStatus, String> {
    crate::models::download_visual_pack()?;
    Ok(crate::models::visual_status())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    /// `"content"` for exact extracted-content matches, `"image"` for
    /// perceptual-hash matches.
    pub kind: String,
    /// Hex content hash or perceptual hash identifying the group.
    pub hash: String,
    pub paths: Vec<String>,
}

/// Finds exact content duplicates and near-duplicate images.
pub fn duplicates_for(
    indexer: &crate::indexer::IndexerManager,
    max_distance: Option<u32>,
    limit: Option<usize>,
) -> Result<Vec<DuplicateGroup>, String> {
    let db = indexer.metadata.read().db();
    let Some(db) = db else {
        return Ok(Vec::new());
    };

    let limit = limit.unwrap_or(200).clamp(1, 1_000);
    let max_distance = max_distance.unwrap_or(10).min(64);
    let mut groups = Vec::new();
    let mut seen: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();
    let group_key = |paths: &[String]| {
        let mut key = paths.to_vec();
        key.sort();
        key
    };

    for (hash, paths) in db.exact_duplicate_groups(limit)? {
        seen.insert(group_key(&paths));
        groups.push(DuplicateGroup {
            kind: "content".to_string(),
            hash,
            paths,
        });
    }

    // Screenshot bursts are the primary version grouping. A near-duplicate
    // group covering exactly the same paths would be redundant.
    for (look, paths) in db.burst_duplicate_groups(limit)? {
        if !seen.insert(group_key(&paths)) {
            continue;
        }
        groups.push(DuplicateGroup {
            kind: "burst".to_string(),
            hash: format!("{:016x}", look),
            paths,
        });
    }

    for (phash, paths) in db.near_duplicate_groups(max_distance, limit)? {
        if !seen.insert(group_key(&paths)) {
            continue;
        }
        groups.push(DuplicateGroup {
            kind: "image".to_string(),
            hash: format!("{:016x}", phash),
            paths,
        });
    }

    Ok(groups)
}

#[tauri::command]
pub async fn find_duplicates(
    state: State<'_, AppState>,
    max_distance: Option<u32>,
    limit: Option<usize>,
) -> Result<Vec<DuplicateGroup>, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.find_duplicates(max_distance, limit);
    }
    duplicates_for(&state.indexer, max_distance, limit)
}

#[tauri::command]
pub async fn find_similar_images(
    state: State<'_, AppState>,
    path: String,
    limit: Option<usize>,
) -> Result<Vec<crate::embed::visual::SimilarImageResult>, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.find_similar_images(&path, limit);
    }
    state
        .indexer
        .find_similar_images(Path::new(&path), limit.unwrap_or(20))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    pub success: bool,
    pub message: String,
}

#[tauri::command]
pub async fn execute_command(
    command: String,
    state: State<'_, AppState>,
) -> Result<CommandOutput, String> {
    let trimmed = command.trim().trim_start_matches(['/', '>']);
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let cmd = parts.first().copied().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "reindex" => {
            if let Some(daemon) = state.daemon.as_ref() {
                daemon.rescan()?;
            } else {
                state.indexer.start_indexing();
            }
            Ok(CommandOutput {
                success: true,
                message: "Reindexing started across configured directories...".to_string(),
            })
        }
        "doctor" => {
            let meta_count = state.indexer.metadata.read().len();
            let config = state.indexer.config.read().clone();
            let roots = config.watched_dirs.len();
            let status = state.indexer.get_status();
            let units_count = state.indexer.unit_store.read().len();
            let msg = format!(
                "Kensaku Doctor Report:\n• Metadata entries: {}\n• Full-text indexed docs: {}\n• Semantic units: {}\n• Configured roots: {}\n• Status: {}\n• Metadata DB: WAL enabled (healthy)",
                meta_count, status.content_indexed, units_count, roots, status.current_status
            );
            Ok(CommandOutput {
                success: true,
                message: msg,
            })
        }
        "model" => {
            let units_count = state.indexer.unit_store.read().len();
            let cfg = state.indexer.config.read().clone();
            let status = if cfg.semantic_enabled {
                "enabled"
            } else {
                "disabled"
            };
            let msg = format!(
                "Active Model: {} (static int8 table, 256-dim, ~5µs/query, offline)\nStatus: {}\nLoaded Units: {}",
                cfg.semantic_model, status, units_count
            );
            Ok(CommandOutput {
                success: true,
                message: msg,
            })
        }
        "add" if parts.get(1).copied() == Some("folder") => Ok(CommandOutput {
            success: true,
            message: "Use the Settings dialog to add new folders to watch, or trigger >settings."
                .to_string(),
        }),
        "dupes" => Ok(CommandOutput {
            success: true,
            message: "DUPLICATES".to_string(),
        }),
        "settings" => Ok(CommandOutput {
            success: true,
            message: "SETTINGS".to_string(),
        }),
        _ => Ok(CommandOutput {
            success: false,
            message: format!(
                "Unknown command '/{}'. Available: /reindex, /doctor, /model, /dupes, /add folder, /settings",
                trimmed
            ),
        }),
    }
}

#[tauri::command]
pub async fn read_file_content(path: String) -> Result<String, String> {
    let p = Path::new(&path);
    crate::indexer::content::read_text_content(p, 256_000)
        .ok_or_else(|| "Could not read text content from file".to_string())
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<KensakuConfig, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.get_config();
    }
    Ok(state.indexer.config.read().clone())
}

#[tauri::command]
pub async fn save_config(config: KensakuConfig, state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    if let Some(daemon) = state.daemon.as_ref() {
        daemon.save_config(config.clone())?;
        *state.indexer.config.write() = config.clone();
    } else {
        state.indexer.apply_config(config.clone())?;
    }
    // Apply window-mode changes to the live window, then re-register the
    // global shortcut so a newly chosen key works without a restart. App mode
    // has no summon shortcut: the window is already in the taskbar.
    if let Some(window) = app.get_webview_window(crate::main_window::LABEL) {
        crate::main_window::apply_mode(&window, &config.app_mode);
    }
    let _ = app.global_shortcut().unregister_all();
    if config.app_mode == "launcher"
        && let Ok(sc) = Shortcut::from_str(&config.shortcut)
    {
        let _ = app.global_shortcut().register(sc);
    }
    Ok(())
}

/// Applies the saved window mode to the live window.
///
/// Setup keeps a normal taskbar window while it indexes (which can take a long
/// time) and calls this when the wizard closes, so the chosen mode only takes
/// effect once setup stops owning the window.
#[tauri::command]
pub async fn apply_window_mode(app: AppHandle) -> Result<(), String> {
    let config = KensakuConfig::load();
    if let Some(window) = app.get_webview_window(crate::main_window::LABEL) {
        crate::main_window::apply_mode(&window, &config.app_mode);
    }
    Ok(())
}

#[tauri::command]
pub async fn is_setup_complete() -> Result<bool, String> {
    Ok(KensakuConfig::is_setup_done())
}

#[tauri::command]
pub async fn is_setup_configured() -> Result<bool, String> {
    Ok(KensakuConfig::load().is_setup_complete)
}

#[tauri::command]
pub async fn run_setup_again() -> Result<(), String> {
    let mut config = KensakuConfig::load();
    config.is_setup_complete = false;
    config.first_scan_done = false;
    config.save()?;
    Ok(())
}

#[tauri::command]
pub async fn complete_setup(
    watched_dirs: Vec<String>,
    excluded_dirs: Vec<String>,
    shortcut: Option<String>,
    ocr_engine: Option<String>,
    app_mode: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    let mut config = KensakuConfig::load();
    config.watched_dirs = watched_dirs.into_iter().map(PathBuf::from).collect();
    config.excluded_dirs = excluded_dirs;
    if let Some(sc) = shortcut
        && !sc.trim().is_empty()
    {
        config.shortcut = sc;
    }
    if let Some(engine) = ocr_engine
        && [crate::ocr::TESSERACT, crate::ocr::ACCURATE, crate::ocr::INDRA].contains(&engine.as_str())
    {
        config.ocr_engine = engine;
    }
    if let Some(mode) = app_mode
        && (mode == "launcher" || mode == "app")
    {
        config.app_mode = mode;
    }
    config.is_setup_complete = true;
    config.first_scan_done = false;
    config.save()?;

    crate::indexer::extract::set_ocr_enabled(config.ocr_enabled);
    crate::indexer::extract::image::set_ocr_max_image_bytes(config.ocr_max_image_bytes);
    crate::indexer::extract::set_ocr_languages(config.ocr_languages.clone());
    crate::indexer::extract::set_content_caps(config.content_max_file_size_mb, config.content_max_text_bytes);
    crate::embed::set_max_semantic_units(config.content_max_semantic_units);
    crate::ocr::set_engine(&config.ocr_engine);
    crate::indexer::extract::set_external_extractors(config.extractors.clone());
    *state.indexer.config.write() = config.clone();
    if let Some(daemon) = state.daemon.as_ref() {
        // In daemon mode this window cannot write the stores; ask the daemon
        // (which owns them) to rescan instead.
        daemon.rescan()?;
    } else {
        state.indexer.start_indexing();
    }

    // The wizard keeps the window in taskbar mode while it indexes. The
    // frontend applies the chosen mode in `apply_window_mode` when setup
    // closes; here we only register the global shortcut in launcher mode.
    let _ = app.global_shortcut().unregister_all();
    if config.app_mode == "launcher"
        && let Ok(sc) = Shortcut::from_str(&config.shortcut)
    {
        let _ = app.global_shortcut().register(sc);
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeItem {
    pub path: String,
    pub filename: String,
    pub size: u64,
    pub modified: u64,
    pub is_dir: bool,
    /// Last time the file was opened through Kensaku (0 = never).
    pub last_open: u64,
    /// Total opens through Kensaku.
    pub opens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeData {
    pub recent: Vec<HomeItem>,
    pub pins: Vec<HomeItem>,
    pub saved_searches: Vec<crate::config::SavedSearch>,
    #[serde(default)]
    pub recent_queries: Vec<String>,
}

pub fn home_data_for(indexer: &crate::indexer::IndexerManager) -> Result<HomeData, String> {
    let metadata = indexer.metadata.read();
    let config = indexer.config.read().clone();
    let mut recent: Vec<HomeItem> = (0..metadata.len())
        .filter_map(|i| {
            let e = metadata.materialize(i as u32);
            if e.is_dir || e.modified == 0 {
                return None;
            }
            let (opens, last_open) = metadata.get_usage(&e.path);
            Some(HomeItem {
                path: e.path.to_string_lossy().to_string(),
                filename: e.name.clone(),
                size: e.size,
                modified: e.modified,
                is_dir: e.is_dir,
                last_open,
                opens,
            })
        })
        .collect();
    // "Recent" means last touched: opening a file through Kensaku counts
    // (that timestamp lives in `last_open`), otherwise its on-disk modified
    // time. Sort by the later of the two, opens as a secondary signal, and
    // the path as the deterministic tie-break — files generated in one batch
    // share timestamps, and without it the tail of the list would reshuffle
    // on every refetch.
    let touched = |item: &HomeItem| item.last_open.max(item.modified);
    recent.sort_by(|a, b| {
        touched(b)
            .cmp(&touched(a))
            .then_with(|| b.opens.cmp(&a.opens))
            .then_with(|| a.path.cmp(&b.path))
    });
    recent.truncate(12);

    let pins = config
        .pins
        .iter()
        .map(|p| {
            let path = std::path::PathBuf::from(p);
            match metadata.get(&path) {
                Some(e) => {
                    let (opens, last_open) = metadata.get_usage(&e.path);
                    HomeItem {
                        path: p.clone(),
                        filename: e.name.clone(),
                        size: e.size,
                        modified: e.modified,
                        is_dir: e.is_dir,
                        last_open,
                        opens,
                    }
                }
                None => HomeItem {
                    path: p.clone(),
                    filename: path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone()),
                    size: 0,
                    modified: 0,
                    is_dir: path.is_dir(),
                    last_open: 0,
                    opens: 0,
                },
            }
        })
        .collect();

    Ok(HomeData {
        recent,
        pins,
        saved_searches: config.saved_searches,
        recent_queries: crate::main_window::recent_queries(),
    })
}

#[tauri::command]
pub async fn get_home_data(state: State<'_, AppState>) -> Result<HomeData, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.get_home_data();
    }
    home_data_for(&state.indexer)
}

pub fn toggle_pin_for(
    indexer: &crate::indexer::IndexerManager,
    path: String,
) -> Result<bool, String> {
    let mut config = indexer.config.read().clone();
    let pinned = if let Some(pos) = config.pins.iter().position(|p| p == &path) {
        config.pins.remove(pos);
        false
    } else {
        config.pins.push(path);
        true
    };
    config.save()?;
    *indexer.config.write() = config;
    Ok(pinned)
}

#[tauri::command]
pub async fn toggle_pin(path: String, state: State<'_, AppState>) -> Result<bool, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.toggle_pin(path);
    }
    toggle_pin_for(&state.indexer, path)
}

/// Directory suggestions for the `@` scope menu, derived from indexed dirs.
pub fn scope_suggestions_for(
    indexer: &crate::indexer::IndexerManager,
    prefix: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<String>, String> {
    let prefix = prefix.unwrap_or_default().trim().to_lowercase();
    let limit = limit.unwrap_or(12).clamp(1, 50);
    let metadata = indexer.metadata.read();
    let mut matches: Vec<(u8, String)> = Vec::new();
    for i in 0..metadata.len() {
        let entry = metadata.materialize(i as u32);
        if !entry.is_dir {
            continue;
        }
        let name = entry.name.to_lowercase();
        let path = entry.path.to_string_lossy().to_string();
        let score = if prefix.is_empty() || name.starts_with(&prefix) {
            0
        } else if name.contains(&prefix) {
            1
        } else if path.to_lowercase().contains(&prefix) {
            2
        } else {
            continue;
        };
        matches.push((score, path));
    }
    matches.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(&b.1))
    });
    Ok(matches
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect())
}

#[tauri::command]
pub async fn get_scope_suggestions(
    prefix: Option<String>,
    limit: Option<usize>,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    if let Some(daemon) = state.daemon.as_ref() {
        return daemon.get_scope_suggestions(prefix, limit);
    }
    scope_suggestions_for(&state.indexer, prefix, limit)
}

#[tauri::command]
pub async fn get_suggested_directories() -> Result<Vec<String>, String> {
    let mut dirs_list = Vec::new();
    if let Some(home) = dirs::home_dir() {
        // Suggestions only: the home folder itself is never preselected. The
        // short list keeps the setup screen scannable — everything else is a
        // folder-picker or a typed path away.
        let candidates = ["Documents", "Downloads", "Desktop", "Pictures"];

        for c in candidates {
            let p = home.join(c);
            if p.exists() {
                dirs_list.push(p.to_string_lossy().to_string());
            }
        }
    }
    Ok(dirs_list)
}

/// Validates a user-typed folder path and returns its canonical form.
///
/// Accepts `~` expansion, quoted paths and stray whitespace, and refuses
/// anything that is not an existing directory — so the setup screen can show
/// a clear error instead of silently watching a typo forever. The canonical
/// form is what `complete_setup` stores and what the UI dedupes against.
#[tauri::command]
pub async fn validate_directory(path: String) -> Result<String, String> {
    let trimmed = path.trim().trim_matches('"').trim().to_string();
    let resolved = if trimmed == "~" {
        dirs::home_dir()
    } else if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        dirs::home_dir().map(|home| home.join(rest))
    } else {
        Some(std::path::PathBuf::from(&trimmed))
    };
    let Some(candidate) = resolved else {
        return Err("Could not resolve the home directory".to_string());
    };
    if !candidate.exists() {
        return Err(format!("Folder not found: {}", candidate.display()));
    }
    if !candidate.is_dir() {
        return Err(format!("Not a folder: {}", candidate.display()));
    }
    let mut canonical = candidate
        .canonicalize()
        .map_err(|e| format!("Could not resolve {}: {}", candidate.display(), e))?
        .to_string_lossy()
        .to_string();
    // Windows canonicalize() returns an extended-length path (\\?\C:\...);
    // every other command and the UI expect the plain form.
    if let Some(stripped) = canonical.strip_prefix("\\\\?\\") {
        canonical = stripped.to_string();
    }
    Ok(canonical)
}

#[tauri::command]
pub async fn hide_window(app_handle: AppHandle) -> Result<(), String> {
    crate::main_window::hide(&app_handle);
    Ok(())
}

/// Keeps the search text so a recreated window restores it.
#[tauri::command]
pub fn remember_query(query: String) {
    crate::main_window::remember_query(&query);
}

#[tauri::command]
pub fn last_query() -> String {
    crate::main_window::last_query()
}

#[tauri::command]
pub fn record_recent_query(query: String) {
    crate::main_window::record_recent_query(&query);
}

#[tauri::command]
pub fn get_recent_queries() -> Vec<String> {
    crate::main_window::recent_queries()
}

#[tauri::command]
pub fn clear_recent_queries() {
    crate::main_window::clear_recent_queries();
}

/// Wipes all Kensaku state on the next launch.
///
/// The marker file is written now and consumed by `maybe_apply_pending_reset`
/// before the restarted process opens the config, database or index. This keeps
/// the removal ordering safe: no open handles exist when the directory is
/// Runs the repair a Health check offers.
///
/// One entry point instead of a command per row: the check names an action,
/// the UI posts that name back, and the backend maps it to the code that
/// already exists. Unknown kinds are rejected rather than silently ignored, so
/// a stale UI cannot look like a successful repair.
#[tauri::command]
pub async fn run_doctor_action(
    action: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<String, String> {
    match action.as_str() {
        "download-models" => {
            crate::models::download_models().await?;
            Ok("Downloading model files. Semantic search upgrades when it finishes.".to_string())
        }
        "download-ocr-pack" => {
            crate::models::download_ocr_pack()?;
            Ok("Downloading the OCR pack.".to_string())
        }
        "download-indra-pack" => {
            crate::models::download_indra_pack()?;
            Ok("Downloading the Indra OCR pack.".to_string())
        }
        "cleanup-storage" => {
            let report = crate::maintenance::cleanup_stale_artifacts();
            Ok(format!(
                "Reclaimed {}{}.",
                crate::doctor::format_bytes(report.bytes_reclaimed),
                if report.errors.is_empty() {
                    String::new()
                } else {
                    format!(" ({} item(s) could not be removed)", report.errors.len())
                }
            ))
        }
        "restart-watcher" => {
            // A fresh crawl restarts the watcher at its end, and re-reads
            // anything that changed while it was down.
            if state.daemon.is_some() {
                return Err(
                    "The index is owned by the Kensaku daemon. Restart it instead.".to_string(),
                );
            }
            state.indexer.clone().rebuild_all()?;
            Ok("Restarting indexing; the watcher comes back when the scan finishes.".to_string())
        }
        "reindex" => {
            if state.daemon.is_some() {
                return Err(
                    "The index is owned by the Kensaku daemon. Restart it instead.".to_string(),
                );
            }
            state.indexer.clone().rebuild_all()?;
            Ok("Rebuilding the index.".to_string())
        }
        "open-crash-log" => {
            let path = crate::crashlog::path();
            if !path.is_file() {
                return Err("The log file no longer exists.".to_string());
            }
            use tauri_plugin_opener::OpenerExt;
            app.opener()
                .open_path(path.display().to_string(), None::<&str>)
                .map_err(|e| format!("Could not open the log: {e}"))?;
            Ok("Opened the log.".to_string())
        }
        other => Err(format!("Unknown repair: {other}")),
    }
}

/// deleted.
#[tauri::command]
pub async fn reset_all(
    app: AppHandle,
    confirm: String,
    remove_packs: Option<bool>,
) -> Result<(), String> {
    if confirm != "RESET" {
        return Err("Reset cancelled: confirmation must be exactly \"RESET\".".to_string());
    }

    let dir = KensakuConfig::config_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create config directory: {}", e))?;
    let marker = dir.join(".reset-requested");
    // The marker's contents carry the choice: the fresh process applies it
    // before anything is open, and keeps downloaded packs unless told "full".
    let scope = if remove_packs.unwrap_or(false) { "full" } else { "keep-packs" };
    std::fs::write(&marker, scope.as_bytes())
        .map_err(|e| format!("Failed to write reset marker: {}", e))?;

    // The replacement process starts before this one exits, so the instance
    // lock has to be handed over first. Without this the fresh instance lost
    // the race, printed "another instance is running" and quit: the app closed
    // and the reset never ran.
    crate::release_instance_lock();

    // Never returns: the process exits and relaunches, and the fresh process
    // applies the pending reset before constructing any application state.
    app.restart();
}

#[tauri::command]
pub async fn rebuild_index(state: State<'_, AppState>) -> Result<(), String> {
    // In daemon mode this window is a read-only client; the daemon owns the
    // stores and there is no safe way to wipe them from here.
    if state.daemon.is_some() {
        return Err(
            "The index is owned by the Kensaku daemon. Stop the daemon, then rebuild.".to_string(),
        );
    }
    state.indexer.clone().rebuild_all()
}

#[tauri::command]
pub async fn get_reset_pending() -> Result<bool, String> {
    Ok(crate::reset_pending())
}

/// Builds the platform autostart handle for the running executable.
///
/// `current_exe()` is the release binary when Kensaku is installed normally. In
/// development (`cargo run`) it resolves to `target/debug/kensaku`, which is a
/// console-subsystem binary on Windows: registered at sign-in it would open a
/// terminal window that stays for the whole session. Dev builds therefore
/// refuse to register (see `set_autostart`) and clear stale entries.
fn autostart_handle() -> Result<AutoLaunch, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("Could not resolve the Kensaku executable path: {}", e))?;
    let exe = exe
        .to_str()
        .ok_or_else(|| "Kensaku executable path is not valid UTF-8".to_string())?;

    // The builder's defaults match the old platform-specific constructors,
    // except Windows: 0.6 defaults to `Dynamic` (which tries HKLM first and
    // can raise a UAC prompt), while the old constructor only wrote HKCU.
    // Keep the current-user entry so enabling autostart stays prompt-free.
    let handle = AutoLaunchBuilder::new()
        .set_app_name("Kensaku")
        .set_app_path(exe)
        .set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser)
        .set_args(&[] as &[&str])
        .build()
        .map_err(|e| format!("Could not configure autostart: {}", e))?;

    Ok(handle)
}

#[tauri::command]
pub async fn get_autostart() -> Result<bool, String> {
    autostart_handle()?
        .is_enabled()
        .map_err(|e| format!("Failed to read autostart state: {}", e))
}

#[tauri::command]
pub async fn set_autostart(enabled: bool) -> Result<(), String> {
    // A dev build is a console binary on Windows: at sign-in it would open a
    // terminal window that remains for the whole session. Only the installed
    // release build may register itself.
    if enabled && cfg!(debug_assertions) {
        eprintln!(
            "[Kensaku] autostart is ignored in dev builds; install the release build for start at sign-in"
        );
        return Ok(());
    }
    let handle = autostart_handle()?;
    let result = if enabled {
        handle.enable()
    } else {
        handle.disable()
    };
    result.map_err(|e| {
        format!(
            "Failed to {} autostart: {}",
            if enabled { "enable" } else { "disable" },
            e
        )
    })
}

/// Removes autostart entries left behind by dev sessions.
///
/// A previous dev run that completed setup registered the debug binary in
/// the user's Run key, so every login opened a console window. Dev builds
/// clear that on startup; release builds never touch it.
pub fn self_heal_autostart() {
    if !cfg!(debug_assertions) {
        return;
    }
    let handle = match autostart_handle() {
        Ok(handle) => handle,
        Err(_) => return,
    };
    match handle.is_enabled() {
        Ok(true) => {
            if let Err(e) = handle.disable() {
                eprintln!("[Kensaku] could not remove the dev autostart entry: {e}");
            } else {
                eprintln!(
                    "[Kensaku] removed the dev-build autostart entry (it opened a console at sign-in)"
                );
            }
        }
        Ok(false) => {}
        Err(_) => {}
    }
}

#[tauri::command]
pub async fn run_doctor(state: State<'_, AppState>) -> Result<crate::doctor::DoctorReport, String> {
    Ok(crate::doctor::run(&state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(extension: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "kensaku-commands-test-{}-{}.{}",
            std::process::id(),
            nanos,
            extension
        ))
    }

    #[test]
    fn image_preview_returns_dimensions_and_no_content() {
        let path = temp_path("png");
        ::image::RgbImage::from_pixel(12, 7, ::image::Rgb([10, 20, 30]))
            .save_with_format(&path, ::image::ImageFormat::Png)
            .expect("write test png");

        let preview = build_file_preview(path.to_str().unwrap()).expect("preview");

        assert_eq!(preview.preview_type, "image");
        assert!(preview.content.is_none(), "images must not be OCR'd");
        assert_eq!((preview.width, preview.height), (Some(12), Some(7)));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn thumbnail_cache_lives_at_config_dir_thumbs() {
        let dir = thumbnail_cache_dir();
        assert_eq!(
            dir.file_name().and_then(|name| name.to_str()),
            Some("thumbs")
        );
        assert_eq!(dir.parent(), Some(KensakuConfig::config_dir().as_path()));
        assert_eq!(
            KensakuConfig::config_dir()
                .file_name()
                .and_then(|name| name.to_str()),
            Some("kensaku")
        );
    }

    #[test]
    fn asset_protocol_scope_includes_the_config_thumbs_dir() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("valid tauri config");
        let scope = config["app"]["security"]["assetProtocol"]["scope"]
            .as_array()
            .expect("asset protocol scope");
        let entries: Vec<&str> = scope.iter().filter_map(|value| value.as_str()).collect();

        assert_eq!(entries.first().copied(), Some("$CONFIG/kensaku/thumbs/**"));
        for expected in [
            "$CONFIG/kensaku/thumbs/**",
            "$APPCONFIG/thumbs/**",
            "$APPLOCALDATA/kensaku/thumbs/**",
            "$APPDATA/kensaku/thumbs/**",
        ] {
            assert!(entries.contains(&expected), "missing scope {expected}");
        }
    }
}

/* ---------------------------------------------------------------- */
/* Local LLM: query compiler and /ask                                */
/* ---------------------------------------------------------------- */

/// Outcome of an LLM compile attempt for the UI.



/* ---------------------------------------------------------------- */
/* One-click agent connect                                           */
/* ---------------------------------------------------------------- */

#[tauri::command]
pub async fn agent_connect_status() -> Result<Vec<crate::agent::connect::ClientStatus>, String> {
    Ok(crate::agent::connect::detect_clients())
}

#[tauri::command]
pub async fn agent_connect_install(
    client: String,
) -> Result<crate::agent::connect::InstallOutcome, String> {
    let parsed = parse_connect_client(&client)?;
    crate::agent::connect::install(parsed)
}

#[tauri::command]
pub async fn agent_connect_snippet(client: String) -> Result<String, String> {
    let parsed = parse_connect_client(&client)?;
    crate::agent::connect::config_snippet(parsed)
}

fn parse_connect_client(client: &str) -> Result<crate::agent::connect::Client, String> {
    crate::agent::connect::Client::ALL
        .into_iter()
        .find(|candidate| candidate.id() == client)
        .ok_or_else(|| format!("unknown client: {}", client))
}

/// Extra facts for the hover card, fetched on demand. Dimensions are read
/// only for image files, so this never runs during search.
#[derive(Debug, Clone, Serialize)]
pub struct FileDetails {
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[tauri::command]
pub async fn get_file_details(path: String) -> Result<FileDetails, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let p = Path::new(&path);
        let is_image = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| crate::indexer::extract::image::is_supported_image_extension(&e.to_ascii_lowercase()))
            .unwrap_or(false);
        let dims = if is_image {
            crate::indexer::extract::image::dimensions(p)
        } else {
            None
        };
        Ok(FileDetails {
            width: dims.map(|(w, _)| w),
            height: dims.map(|(_, h)| h),
        })
    })
    .await
    .map_err(|e| format!("Details task failed: {}", e))?
}

/// Thumbnail bytes as a `data:` URL. Using a data URL means the card never
/// depends on the asset protocol or its scope, so it renders the same on every
/// platform. Previews are 240px JPEGs (a few KB), which keeps the payload
/// small for the visible cards.
#[tauri::command]
pub async fn get_thumbnail_data(path: String) -> Result<String, String> {
    use base64::Engine;
    tauri::async_runtime::spawn_blocking(move || {
        let thumb = generate_thumbnail(&path)?;
        let bytes = std::fs::read(&thumb)
            .map_err(|e| format!("Failed to read thumbnail: {}", e))?;
        Ok(format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    })
    .await
    .map_err(|e| format!("Thumbnail task failed: {}", e))?
}

/// Checks GitHub for new Kensaku releases.
#[tauri::command]
pub async fn check_for_updates() -> Result<crate::updater::UpdateInfo, String> {
    tauri::async_runtime::spawn_blocking(crate::updater::check_for_update)
        .await
        .map_err(|e| format!("Update check task failed: {}", e))?
}

/// Downloads the specified update asset in the background.
#[tauri::command]
pub async fn download_update(asset_url: String, asset_name: String) -> Result<crate::updater::UpdateStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        crate::updater::download_update(&asset_url, &asset_name)
    })
    .await
    .map_err(|e| format!("Update download task failed: {}", e))?
}

/// Polls progress of an in-progress update download.
#[tauri::command]
pub async fn get_updater_status() -> Result<crate::updater::UpdateStatus, String> {
    Ok(crate::updater::get_status())
}

/// Spawns the downloaded installer detached and shuts down the current app instance.
#[tauri::command]
pub async fn install_update_and_restart(app: AppHandle) -> Result<(), String> {
    crate::updater::install_and_restart(&app)
}
