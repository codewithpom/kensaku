pub mod arena;
pub mod burst;
pub mod content;
pub mod extract;
pub mod ignore;
pub mod metadata;
pub mod metadata_db;
pub mod tokenizer;
pub mod walker;
pub mod watcher;

use crate::config::KensakuConfig;
use crate::embed::units::chunk_text;
use crate::embed::{EMBEDDING_DIM, EmbeddingQueue, ModelRegistry, Unit, UnitStore};
use crate::indexer::content::{
    ContentIndex, is_indexable_extension, is_indexable_file_name, read_text_content_checked,
};
use crate::indexer::metadata::MetadataStore;
use crate::indexer::walker::{FileEntry, is_cloud_placeholder_path, walk_directories};
use crate::indexer::watcher::{WatcherHandle, start_file_watcher};
use crate::search::{SearchDispatcher, SearchResponse, SortOrder};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingStatus {
    pub is_indexing: bool,
    pub files_indexed: usize,
    pub content_indexed: usize,
    pub current_status: String,
    #[serde(default)]
    pub is_embedding: bool,
    #[serde(default)]
    pub units_indexed: usize,
    #[serde(default)]
    pub semantic_ready: bool,
    /// Coarse pipeline phase: "idle", "walking", "extracting", "embedding", "committing".
    #[serde(default = "default_phase")]
    pub phase: String,
    /// Total entries discovered by the current crawl (0 until the walk ends).
    #[serde(default)]
    pub total_files: usize,
    /// Files whose text extraction has finished in the current crawl.
    #[serde(default)]
    pub extract_done: usize,
    /// Files queued for text extraction in the current crawl.
    #[serde(default)]
    pub extract_total: usize,
    /// True while the user has paused background indexing.
    #[serde(default)]
    pub paused: bool,
}

fn default_phase() -> String {
    "idle".to_string()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_unix_secs() -> u64 {
    now_unix_ms() / 1000
}

/// True when an existing entry is unchanged and has no content re-read pending.
///
/// A pending entry (set by an OCR toggle) is always re-processed even when its
/// mtime and size still match, which is how the image re-scan stays targeted.
fn is_unchanged(existing: &FileEntry, entry: &FileEntry) -> bool {
    // (0, 0) is the walker's "I could not stat this" sentinel, not a file
    // state: a Windows file locked by another process, or one being scanned by
    // antivirus, fails `metadata()` for that moment. Treating it as a change
    // did two harmful things — it re-read the file, and it *stored* 0/0 as the
    // file's size and modification time, so the next scan saw different values
    // and re-read it again. One transient lock produced churn across launches.
    if entry.size == 0 && entry.modified == 0 {
        return true;
    }
    existing.modified == entry.modified && existing.size == entry.size && !existing.content_pending
}

/// One file scheduled for text extraction in the current crawl.
struct ExtractJob {
    node_id: u64,
    path: PathBuf,
    name: String,
    modified: u64,
    size: u64,
    is_image: bool,
    /// The pixels changed (or the row is new), so any stored dHash is stale.
    phash_needs_refresh: bool,
}

/// Crawl order: text before images, newest first within each class, then size.
fn extract_sort_key(job: &ExtractJob) -> (bool, Reverse<u64>, u64) {
    (job.is_image, Reverse(job.modified), job.size)
}

/// A file that has been extracted, together with any semantic vectors.
///
/// Passed from the extraction workers to the single index-committing thread
/// through a bounded channel. The bound is what keeps peak memory flat: text is
/// dropped as soon as it has been written to the lexical and semantic indexes
/// instead of every document's text accumulating in a `Vec` until the end.
struct ExtractedWork {
    node_id: u64,
    path: PathBuf,
    name: String,
    modified: u64,
    size: u64,
    content: Option<String>,
    units: Vec<(usize, usize, Vec<i8>)>,
    content_hash: Option<String>,
    /// Present for images, even when OCR produced no text.
    image_sig: Option<Vec<u8>>,
    /// Opportunistic hash from an already-fresh cached thumbnail.
    image_phash: Option<u64>,
    /// Whether a missing opportunistic hash must invalidate the stored dHash.
    phash_needs_refresh: bool,
    /// Extraction did not finish (timeout, busy, OCR not runnable yet): keep
    /// the file pending and try again next crawl.
    retry: bool,
}

/// How many crawls a file may fail extraction on before it is left alone
/// until it changes. Timeouts and slot saturation are usually persistent for
/// a given file (a huge scan, a locked network file), so an unbounded retry
/// just repeats the same cost on every launch.
const MAX_EXTRACTION_ATTEMPTS: u32 = 3;

fn fnv_hash(data: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// True while a perceptual-hash backfill thread is running.
static PHASH_BACKFILL_RUNNING: AtomicBool = AtomicBool::new(false);

/// Computes perceptual hashes for images the crawl deliberately skipped.
///
/// Full-image decoding is the slowest and most memory-hungry part of image
/// handling, and the hash is only used by the `/dupes` view — it must never
/// gate OCR or the search index. This worker decodes one image at a time,
/// pauses during crawls and while a query is in flight, and lets hashes appear
/// progressively in duplicate grouping.
fn start_phash_backfill(
    metadata: Arc<RwLock<MetadataStore>>,
    content_index: Arc<RwLock<Option<ContentIndex>>>,
    visual_store: Arc<RwLock<crate::embed::visual::VisualStore>>,
    crawl_running: Arc<AtomicBool>,
    query_in_flight: Arc<AtomicBool>,
) {
    if PHASH_BACKFILL_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }

    thread::spawn(move || {
        let thumbs_dir = KensakuConfig::config_dir().join("thumbs");
        struct ResetOnExit;
        impl Drop for ResetOnExit {
            fn drop(&mut self) {
                PHASH_BACKFILL_RUNNING.store(false, Ordering::SeqCst);
            }
        }
        let _reset = ResetOnExit;

        let Some(db) = metadata.read().db() else {
            return;
        };

        loop {
            // Never compete with a crawl or an interactive query.
            while crawl_running.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(500));
            }
            while query_in_flight.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(100));
            }

            let paths = match db.image_paths_missing_phash(16) {
                Ok(paths) => paths,
                Err(_) => return,
            };
            if !paths.is_empty() {
                for path in paths {
                    if crawl_running.load(Ordering::Relaxed) {
                        break;
                    }
                    if query_in_flight.load(Ordering::Relaxed) {
                        break;
                    }
                    let hash = crate::indexer::extract::image::phash64_cached(&path, &thumbs_dir)
                        .unwrap_or(u64::MAX);
                    let _ = db.set_image_phash(&path, hash);

                    if hash != u64::MAX {
                        let thumb =
                            crate::indexer::extract::image::thumbnail_path(&path, &thumbs_dir);
                        if thumb.is_file()
                            && let Ok(Some(node_id)) = db.get_node_id_by_path(&path)
                        {
                            let needs = visual_store.read().needs_embedding(node_id, hash);
                            if needs
                                && let Some(encoder) = crate::embed::visual::active_encoder()
                                && let Some(vec) = encoder.embed_image_from_thumbnail(&thumb)
                            {
                                visual_store.write().add_or_update(node_id, hash, &vec);
                                let _ =
                                    db.save_visual_unit(node_id, encoder.model_id(), hash, &vec);
                            }
                        }
                    }

                    crate::util::memory::release_memory();
                    // One decode at a time, with a small yield between images so
                    // typing stays responsive.
                    thread::sleep(std::time::Duration::from_millis(20));
                }
                continue;
            }

            // Hashes are complete. Now assign bursts to old rows in small
            // batches. Text comes from the committed content index, so this
            // also works for images indexed before burst grouping existed.
            let paths = match db.image_paths_missing_burst(50) {
                Ok(paths) => paths,
                Err(_) => return,
            };
            if paths.is_empty() {
                return;
            }
            for path in paths {
                if crawl_running.load(Ordering::Relaxed) {
                    break;
                }
                let signature = content_index
                    .read()
                    .as_ref()
                    .and_then(|index| index.get_stored_text(&path))
                    .map(|text| crate::indexer::burst::line_signature(&text))
                    .unwrap_or_default();
                let Ok(updates) = db.set_image_text_sig(&path, &signature) else {
                    return;
                };
                if !updates.is_empty() {
                    metadata.write().apply_bursts(&updates);
                }
                let Ok(updates) = db.assign_burst_for_path(&path) else {
                    return;
                };
                metadata.write().apply_bursts(&updates);
                crate::util::memory::release_memory();
                thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    });
}

pub struct IndexerManager {
    pub metadata: Arc<RwLock<MetadataStore>>,
    pub content_index: Arc<RwLock<Option<ContentIndex>>>,
    pub unit_store: Arc<RwLock<UnitStore>>,
    pub visual_store: Arc<RwLock<crate::embed::visual::VisualStore>>,
    pub embedding_queue: Arc<Mutex<Option<EmbeddingQueue>>>,
    pub query_in_flight: Arc<AtomicBool>,
    pub config: Arc<RwLock<KensakuConfig>>,
    pub status: Arc<RwLock<IndexingStatus>>,
    /// Unix timestamp (ms) when the current/last crawl started, 0 if none.
    pub crawl_start_unix_ms: Arc<AtomicU64>,
    /// Unix timestamp (secs) of the last successful reconcile, 0 if never.
    pub last_reconcile_unix_secs: Arc<AtomicU64>,
    /// True while a live filesystem watcher is running.
    pub watcher_ok: Arc<AtomicBool>,
    is_running: Arc<AtomicBool>,
    watcher_handle: Arc<Mutex<Option<WatcherHandle>>>,
    app_handle: Arc<RwLock<Option<tauri::AppHandle>>>,
}

impl IndexerManager {
    pub fn new(config: KensakuConfig) -> Self {
        // Apply the opt-in external extractor registry before any crawl starts.
        crate::indexer::extract::set_external_extractors(config.extractors.clone());
        crate::indexer::extract::image::set_ocr_max_image_bytes(config.ocr_max_image_bytes);
        crate::indexer::extract::set_content_caps(config.content_max_file_size_mb, config.content_max_text_bytes);
        crate::embed::set_max_semantic_units(config.content_max_semantic_units);
        let index_dir = KensakuConfig::index_dir();
        let content_index = match ContentIndex::new(&index_dir) {
            Ok(idx) => Some(idx),
            Err(e) => {
                crate::say_err!("[Kensaku] Failed to create Tantivy index: {}", e);
                None
            }
        };
        let content_rebuilt = content_index.as_ref().is_some_and(|index| index.rebuilt);

        let metadata_store = Arc::new(RwLock::new(MetadataStore::new()));
        let unit_store = Arc::new(RwLock::new(UnitStore::new(EMBEDDING_DIM)));
        let visual_store = Arc::new(RwLock::new(crate::embed::visual::VisualStore::new(
            crate::embed::visual::VISUAL_DIM,
            crate::embed::visual::MOCK_VISUAL_MODEL_ID,
        )));
        let query_in_flight = Arc::new(AtomicBool::new(false));

        // Semantic state (the ~30 MB embedding table and the persisted unit
        // vectors) is only loaded when semantic search is actually enabled.
        // With it off the indexer stays a plain lexical index and keeps that
        // memory free.
        let semantic_enabled = config.semantic_enabled;

        // Discard semantic units produced by a different embedding model; the
        // embedding worker re-queues every node once it starts.
        if semantic_enabled && let Some(db) = metadata_store.read().db() {
            let expected_model_id = ModelRegistry::global().model_id;
            match db.units_model_id() {
                Ok(Some(existing_model_id)) if existing_model_id != expected_model_id => {
                    match db.reset_semantic_index() {
                        Ok(()) => {
                            unit_store.write().clear();
                            log::info!(
                                "[Kensaku] Semantic model changed (units id {} -> {}); reset semantic index",
                                existing_model_id,
                                expected_model_id
                            );
                        }
                        Err(e) => {
                            crate::say_err!("[Kensaku] Failed to reset semantic index: {}", e);
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    crate::say_err!("[Kensaku] Failed to read semantic model id: {}", e);
                }
            }
        }

        // Load persisted units from database. These are required for semantic
        // search even if the user toggles the tier on later in this session, so
        // they are always restored; only the embedding table is lazy.
        let mut units_count = 0;
        if let Some(db) = metadata_store.read().db() {
            let mut store = unit_store.write();
            match db.load_units_into(&mut store) {
                Ok(count) => {
                    units_count = count;
                    if units_count > 0 {
                        crate::say!(
                            "[Kensaku] Loaded {} semantic units from database",
                            units_count
                        );
                    }
                }
                Err(e) => {
                    crate::say_err!("[Kensaku] Failed to load semantic units: {}", e);
                }
            }
            let mut vstore = visual_store.write();
            let _ = db.load_visual_units_into(&mut vstore);
        }

        if content_rebuilt {
            let marked = metadata_store.write().mark_all_content_pending();
            crate::say_err!(
                "[Kensaku] Content index schema upgraded; {} file(s) queued for re-indexing",
                marked
            );
        }

        Self {
            metadata: metadata_store,
            content_index: Arc::new(RwLock::new(content_index)),
            unit_store,
            visual_store,
            embedding_queue: Arc::new(Mutex::new(None)),
            query_in_flight,
            config: Arc::new(RwLock::new(config)),
            status: Arc::new(RwLock::new(IndexingStatus {
                is_indexing: false,
                files_indexed: 0,
                content_indexed: 0,
                current_status: "Idle".to_string(),
                is_embedding: false,
                units_indexed: units_count,
                semantic_ready: units_count > 0,
                phase: "idle".to_string(),
                total_files: 0,
                extract_done: 0,
                extract_total: 0,
                paused: false,
            })),
            crawl_start_unix_ms: Arc::new(AtomicU64::new(0)),
            last_reconcile_unix_secs: Arc::new(AtomicU64::new(0)),
            watcher_ok: Arc::new(AtomicBool::new(false)),
            is_running: Arc::new(AtomicBool::new(false)),
            watcher_handle: Arc::new(Mutex::new(None)),
            app_handle: Arc::new(RwLock::new(None)),
        }
    }

    /// Read-only handle used by GUI clients that talk to a daemon. It loads the
    /// persisted metadata and Tantivy index without taking a writer lock or
    /// starting a watcher, so it can coexist with the daemon.
    pub fn new_readonly(config: KensakuConfig) -> Self {
        crate::indexer::extract::set_external_extractors(config.extractors.clone());
        crate::indexer::extract::image::set_ocr_max_image_bytes(config.ocr_max_image_bytes);
        crate::indexer::extract::set_content_caps(config.content_max_file_size_mb, config.content_max_text_bytes);
        crate::embed::set_max_semantic_units(config.content_max_semantic_units);
        let index_dir = KensakuConfig::index_dir();
        let content_index = ContentIndex::open_read_only(&index_dir).ok();
        let metadata_store = Arc::new(RwLock::new(MetadataStore::new()));
        let unit_store = Arc::new(RwLock::new(UnitStore::new(EMBEDDING_DIM)));
        let visual_store = Arc::new(RwLock::new(crate::embed::visual::VisualStore::new(
            crate::embed::visual::VISUAL_DIM,
            crate::embed::visual::MOCK_VISUAL_MODEL_ID,
        )));
        let query_in_flight = Arc::new(AtomicBool::new(false));

        let mut units_count = 0;
        if let Some(db) = metadata_store.read().db() {
            let mut store = unit_store.write();
            if let Ok(count) = db.load_units_into(&mut store) {
                units_count = count;
            }
            let mut vstore = visual_store.write();
            let _ = db.load_visual_units_into(&mut vstore);
        }

        Self {
            metadata: metadata_store,
            content_index: Arc::new(RwLock::new(content_index)),
            unit_store,
            visual_store,
            embedding_queue: Arc::new(Mutex::new(None)),
            query_in_flight,
            config: Arc::new(RwLock::new(config)),
            status: Arc::new(RwLock::new(IndexingStatus {
                is_indexing: false,
                files_indexed: 0,
                content_indexed: 0,
                current_status: "Daemon-backed (read-only client)".to_string(),
                is_embedding: false,
                units_indexed: units_count,
                semantic_ready: units_count > 0,
                phase: "idle".to_string(),
                total_files: 0,
                extract_done: 0,
                extract_total: 0,
                paused: false,
            })),
            crawl_start_unix_ms: Arc::new(AtomicU64::new(0)),
            last_reconcile_unix_secs: Arc::new(AtomicU64::new(0)),
            watcher_ok: Arc::new(AtomicBool::new(false)),
            is_running: Arc::new(AtomicBool::new(false)),
            watcher_handle: Arc::new(Mutex::new(None)),
            app_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_app_handle(&self, handle: tauri::AppHandle) {
        *self.app_handle.write() = Some(handle);
    }

    fn emit_status(app_handle: &Arc<RwLock<Option<tauri::AppHandle>>>, status: &IndexingStatus) {
        if let Some(handle) = app_handle.read().as_ref() {
            // The launcher destroys its window when it hides, and the crawl
            // keeps running: emitting then posts to a dead HWND, which WebView2
            // reports as "PostMessage failed ... Invalid window handle" once per
            // update, for the whole remaining crawl. Nothing can receive the
            // event, so skip it.
            use tauri::Manager as _;
            if handle
                .get_webview_window(crate::main_window::LABEL)
                .is_none()
            {
                return;
            }
            use tauri::Emitter;
            let _ = handle.emit("indexing-status", status);
        }
    }

    /// Blocks extraction workers while the user has paused indexing.
    ///
    /// The watcher is a separate thread and keeps applying renames/deletes, so
    /// pausing never makes the index stale about files that disappeared.
    fn wait_while_paused(
        config: &Arc<RwLock<KensakuConfig>>,
        status: &Arc<RwLock<IndexingStatus>>,
        app_handle: &Arc<RwLock<Option<tauri::AppHandle>>>,
    ) {
        loop {
            let paused = config.read().pause_indexing;
            if !paused {
                let mut st = status.write();
                if st.paused {
                    st.paused = false;
                    st.phase = "extracting".to_string();
                    st.current_status = "Resuming indexing…".to_string();
                    Self::emit_status(app_handle, &st);
                }
                return;
            }

            let mut st = status.write();
            if !st.paused {
                st.paused = true;
                st.phase = "paused".to_string();
                st.current_status = "Indexing paused".to_string();
                Self::emit_status(app_handle, &st);
            }
            drop(st);
            thread::sleep(Duration::from_millis(150));
        }
    }

    /// Holds extraction while the user is searching.
    ///
    /// A crawl uses every core it can, and a search that arrives mid-crawl
    /// competes with 2-8 extraction threads for them, which is why the app
    /// felt worst exactly when it was doing the most work. The phash backfill
    /// already yields this way; the extraction pool did not.
    ///
    /// Polling rather than blocking on a condition variable: a flag left set
    /// by a panic in the query path costs a slower crawl, never a hung one.
    fn yield_to_queries(query_in_flight: &Arc<AtomicBool>) {
        if !query_in_flight.load(Ordering::Relaxed) {
            return;
        }
        // Long enough for the query to finish, short enough that the crawl
        // resumes immediately after it.
        while query_in_flight.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Eco mode limits how often extraction jobs start while on battery.
    /// Documents already in flight finish normally; fresh watcher events are
    /// handled by the watcher thread and are not paced.
    fn pace_on_battery(
        config: &Arc<RwLock<KensakuConfig>>,
        battery: &Mutex<(Instant, bool)>,
        gate: &Mutex<Instant>,
    ) {
        let crawl_mode = config.read().crawl_mode;
        let should_pace = {
            let mut cached = battery.lock();
            if cached.0.elapsed() >= Duration::from_secs(30) {
                cached.1 = crate::util::power::should_pace(crawl_mode);
                cached.0 = Instant::now();
            }
            cached.1
        };
        if !should_pace {
            return;
        }

        let mut last = gate.lock();
        let interval = Duration::from_millis(75);
        if let Some(wait) = interval.checked_sub(last.elapsed()) {
            thread::sleep(wait);
        }
        *last = Instant::now();
    }

    pub fn start_indexing(&self) {
        // Never crawl before the setup wizard has run: a fresh or corrupt
        // config falls back to the default roots (the home directory), and
        // scanning/watching those before the user has chosen their folders
        // is exactly the wrong behavior (it once went through `~\AppData`
        // and network/MTP devices on Windows).
        if !self.config.read().is_setup_complete {
            return;
        }
        if !self
            .content_index
            .read()
            .as_ref()
            .map(|index| index.is_writable())
            .unwrap_or(false)
        {
            return;
        }
        if self.is_running.swap(true, Ordering::SeqCst) {
            // Already running
            return;
        }

        // Stop any watcher left over from a previous indexing run so we never
        // end up with duplicate watchers on the same directories.
        let previous_watcher = self.watcher_handle.lock().take();
        if let Some(mut handle) = previous_watcher {
            handle.stop();
        }
        self.watcher_ok.store(false, Ordering::SeqCst);

        // The crawl embeds newly extracted text inline, so a background
        // embedding worker from a previous run must not also pick up the same
        // nodes (it would re-read every file and duplicate units). It is
        // stopped inside the crawl thread so the IPC call returns immediately,
        // and restarted at the end for incremental work.

        let metadata = Arc::clone(&self.metadata);
        let content_index = Arc::clone(&self.content_index);
        let unit_store = Arc::clone(&self.unit_store);
        let visual_store = Arc::clone(&self.visual_store);
        let embedding_queue = Arc::clone(&self.embedding_queue);
        let query_in_flight = Arc::clone(&self.query_in_flight);
        let config_arc = Arc::clone(&self.config);
        let status = Arc::clone(&self.status);
        let is_running = Arc::clone(&self.is_running);
        let watcher_handle = Arc::clone(&self.watcher_handle);
        let app_handle = Arc::clone(&self.app_handle);
        let crawl_start_unix_ms = Arc::clone(&self.crawl_start_unix_ms);
        let last_reconcile_unix_secs = Arc::clone(&self.last_reconcile_unix_secs);
        let watcher_ok = Arc::clone(&self.watcher_ok);

        thread::spawn(move || {
            // A panic anywhere in the crawl (an extractor, a rayon scope, an
            // embedding call) used to leave the gate set for the rest of the
            // process lifetime: every later crawl and rebuild returned
            // immediately and the index never updated again, with no error
            // shown. The guard hands the gate back on unwind; the normal path
            // releases it explicitly and disarms this.
            struct GateGuard {
                gate: Arc<std::sync::atomic::AtomicBool>,
                armed: bool,
            }
            impl Drop for GateGuard {
                fn drop(&mut self) {
                    if self.armed {
                        self.gate.store(false, Ordering::SeqCst);
                    }
                }
            }
            let mut gate_guard = GateGuard {
                gate: Arc::clone(&is_running),
                armed: true,
            };

            // Apply OCR engine selection and record whether it re-queued
            // images: that is one of the reasons a crawl re-reads files, and
            // the report below has to say why.
            // Captured before the sync writes the new value: a report that
            // re-reads the file afterwards always shows "no change".
            let ocr_signature_before = read_ocr_signature();
            let ocr_queued = Self::sync_ocr_state(&config_arc, &metadata);

            let (watched_dirs, excluded_dirs, excluded_exts, crawl_mode, include_ignored) = {
                let cfg = config_arc.read();
                (
                    cfg.watched_dirs.clone(),
                    cfg.excluded_dirs.clone(),
                    cfg.excluded_extensions.clone(),
                    cfg.crawl_mode,
                    cfg.include_ignored,
                )
            };

            // Balanced/eco keep the crawl thread in the OS idle class; full
            // leaves it at normal priority. Failures are advisory.
            crate::util::priority::apply(crawl_mode);

            crawl_start_unix_ms.store(now_unix_ms(), Ordering::Relaxed);
            watcher_ok.store(false, Ordering::Relaxed);

            {
                let mut st = status.write();
                st.is_indexing = true;
                st.paused = false;
                st.phase = "walking".to_string();
                st.current_status = "Scanning files...".to_string();
                Self::emit_status(&app_handle, &st);
            }

            // Stop a background embedding worker from a previous run before the
            // crawl starts writing. This runs here (not on the IPC thread) so a
            // long embedding batch cannot block the caller. The crawl embeds
            // newly extracted text inline; the worker is restarted afterwards
            // for incremental work.
            if let Some(mut queue) = embedding_queue.lock().take() {
                queue.stop();
            }

            crate::say!(
                "[Kensaku Indexer] Starting directory scan across {:?}",
                watched_dirs
            );

            let mut files_count: usize = 0;
            let mut content_count = 0;
            // Why files were re-read, counted rather than guessed. A launch
            // that re-reads thousands of unchanged files is only diagnosable
            // if the crawl can say which bucket they fell into.
            let mut new_files = 0usize;
            let mut restamped_files = 0usize;
            let mut pending_files = 0usize;
            let mut pending_commits = 0;
            let mut crawled_paths: HashSet<PathBuf> = HashSet::new();
            // Images flagged by an OCR-toggle re-scan that were processed by
            // this crawl. Their pending flag is cleared once extraction ran.
            let mut pending_images: HashSet<PathBuf> = HashSet::new();

            // Begin SQLite transaction for batched writes (100x faster than per-file commits)
            if let Some(db) = metadata.read().db() {
                let _ = db.begin_transaction();
            }

            // Phase 1: Collect all entries (jwalk is already parallel internally).
            let all_entries = walk_directories(&watched_dirs, &excluded_dirs, include_ignored);
            let total_entries = all_entries.len();
            {
                let mut st = status.write();
                st.total_files = total_entries;
                st.phase = "walking".to_string();
                st.current_status = format!("Indexing metadata for {} entries...", total_entries);
                Self::emit_status(&app_handle, &st);
            }

            // Phase 2a: Filter excluded extensions, check incremental, add metadata.
            // Collect indexable files for parallel content extraction.
            let mut to_extract: Vec<ExtractJob> = Vec::new();

            for entry in all_entries {
                files_count += 1;

                // Skip files whose extension is in the excluded list.
                //
                // The path is deliberately NOT marked as crawled: marking it
                // meant a file indexed before its extension was excluded was
                // never re-read and never removed, so the exclusion only ever
                // applied to files that did not exist yet.
                if !entry.is_dir
                    && let Some(ext) = entry.extension
                    && excluded_exts.iter().any(|e| e.eq_ignore_ascii_case(ext))
                {
                    continue;
                }

                crawled_paths.insert(entry.path.clone());

                // Incremental crawl: a cheap read-lock check first, so an
                // unchanged tree never contends on the write lock or touches
                // SQLite. A pending entry is re-processed even when mtime/size
                // match so an OCR toggle only re-scans images.
                let previous = { metadata.read().get(&entry.path) };
                let unchanged = previous
                    .as_ref()
                    .is_some_and(|existing| is_unchanged(existing, &entry));
                if unchanged {
                    continue;
                }
                let was_pending = previous
                    .as_ref()
                    .is_some_and(|existing| existing.content_pending);
                match previous.as_ref() {
                    None => new_files += 1,
                    Some(existing) => {
                        if was_pending {
                            pending_files += 1;
                        } else if existing.modified != entry.modified
                            || existing.size != entry.size
                        {
                            restamped_files += 1;
                        }
                    }
                }
                let phash_needs_refresh = previous.as_ref().is_none_or(|existing| {
                    existing.modified != entry.modified || existing.size != entry.size
                });
                let node_id = metadata.write().add(entry.clone());
                if was_pending {
                    pending_images.insert(entry.path.clone());
                }

                // Queue indexable files for parallel content extraction.
                // Cloud placeholders stay metadata-only so indexing does not
                // hydrate (download) them.
                if !entry.is_dir
                    && (is_indexable_extension(entry.extension)
                        || is_indexable_file_name(&entry.name)
                        || crate::indexer::extract::external::handles(&entry.path))
                    && !is_cloud_placeholder_path(&entry.path)
                {
                    to_extract.push(ExtractJob {
                        node_id,
                        path: entry.path.clone(),
                        name: entry.name.clone(),
                        modified: entry.modified,
                        size: entry.size,
                        is_image: entry.extension.is_some_and(
                            crate::indexer::extract::image::is_supported_image_extension,
                        ),
                        phash_needs_refresh,
                    });
                }

                if files_count.is_multiple_of(500) {
                    let mut st = status.write();
                    st.files_indexed = files_count;
                    st.content_indexed = content_count;
                    st.current_status = format!("Indexed {} items", files_count);
                    Self::emit_status(&app_handle, &st);
                }
            }

            // Index text and small files first. Images (OCR) are the slow tail,
            // so this makes the lexical index useful earlier in the crawl while
            // the same total amount of work is done. Within each class, newest
            // files win so a just-saved document is searchable first.
            to_extract.sort_unstable_by_key(extract_sort_key);

            // Phase 2b: streaming extraction → lexical index → semantic index.
            //
            // Previously every extracted document was collected into a Vec and
            // indexed afterwards, and the semantic worker then re-read and
            // re-extracted every file (re-running OCR). The pipeline below
            // extracts each file exactly once and hands it to both indexes
            // through a bounded channel, so peak memory stays flat and images
            // are OCR'd once instead of twice.
            let extract_total = to_extract.len();
            {
                let mut st = status.write();
                st.phase = "extracting".to_string();
                st.files_indexed = files_count;
                st.content_indexed = content_count;
                st.total_files = total_entries;
                st.extract_total = extract_total;
                st.extract_done = 0;
                st.current_status =
                    format!("Extracting text from {} potential files...", extract_total);
                Self::emit_status(&app_handle, &st);
            }

            let semantic_enabled = config_arc.read().semantic_enabled;
            let model = semantic_enabled.then(|| ModelRegistry::global().clone());
            let embed_model_id = model.as_ref().map(|m| m.model_id);

            // Drop semantic units for every file about to be re-extracted. New
            // files have none, so a first scan does no work here. One batched
            // pass (instead of per-file vector compaction) keeps re-indexing a
            // large tree linear.
            if semantic_enabled && !to_extract.is_empty() {
                let ids: HashSet<u64> = to_extract
                    .iter()
                    .map(|job| job.node_id)
                    .filter(|id| *id > 0)
                    .collect();
                if !ids.is_empty() {
                    let id_vec: Vec<u64> = ids.iter().copied().collect();
                    if let Some(db) = metadata.read().db() {
                        let _ = db.delete_units_for_nodes(&id_vec);
                        let _ = db.delete_visual_units_for_nodes(&id_vec);
                    }
                    unit_store.write().remove_units_for_nodes(&ids);
                    for id in &ids {
                        visual_store.write().remove_node(*id);
                    }
                }
            }

            let extract_done = AtomicUsize::new(0);
            let mut extracted_paths: HashSet<PathBuf> = HashSet::new();
            // Files whose extraction did not finish this crawl. They keep (or
            // gain) the pending flag so the next crawl retries them; before,
            // a timed-out or OCR-less image was recorded as "no text" forever.
            let mut retry_paths: HashSet<PathBuf> = HashSet::new();
            let mut units_added = 0usize;

            // Bounded handoff: at most 32 extracted documents (and their
            // vectors) are in flight at any moment.
            let (tx, rx) = std::sync::mpsc::sync_channel::<ExtractedWork>(32);
            let image_db = metadata.read().db();
            let db_handle = metadata.read().db();
            let thumbs_dir = KensakuConfig::config_dir().join("thumbs");
            // The producer task runs on the rayon pool and must own its handles;
            // the consumer (this thread) keeps using the originals.
            let producer_status = Arc::clone(&status);
            let producer_app = Arc::clone(&app_handle);
            let producer_config = Arc::clone(&config_arc);
            let battery_state = Arc::new(Mutex::new((Instant::now(), false)));
            let pace_gate = Arc::new(Mutex::new(Instant::now()));
            let producer_query_flag = Arc::clone(&query_in_flight);

            rayon::scope(|scope| {
                use rayon::prelude::*;

                scope.spawn(move |_| {
                    to_extract.par_iter().for_each_with(tx, |tx, job| {
                        Self::wait_while_paused(&producer_config, &producer_status, &producer_app);
                        // Search comes first: a user waiting on results must not
                        // queue behind a background scan.
                        Self::yield_to_queries(&producer_query_flag);
                        Self::pace_on_battery(&producer_config, &battery_state, &pace_gate);

                        // Image dimensions come from the file header, which is
                        // cheap. Perceptual hashing needs a full decode; doing
                        // it here would serialize the whole crawl behind a
                        // handful of decode slots and starve OCR. The hash is
                        // filled in later by the background backfill.
                        if job.is_image
                            && let Some((width, height)) =
                                crate::indexer::extract::image::dimensions(&job.path)
                            && let Some(db) = image_db.as_ref()
                        {
                            let _ = db.set_image_dimensions(&job.path, width, height);
                        }

                        // The configured text cap (`content_max_text_bytes`),
                        // not a fixed 1 MB: a hard-coded limit here made the
                        // "deep" content setting unable to go past 1 MB.
                        let (content, retry) = read_text_content_checked(
                            &job.path,
                            crate::indexer::extract::content_max_text_bytes() as u64,
                        );
                        let image_sig = job.is_image.then(|| {
                            content
                                .as_deref()
                                .map(crate::indexer::burst::line_signature)
                                .unwrap_or_default()
                        });
                        let image_phash = if job.is_image {
                            crate::indexer::extract::image::phash64_reusing_fresh_thumbnail(
                                &job.path,
                                &thumbs_dir,
                            )
                        } else {
                            None
                        };

                        // Embed here, in parallel with extraction and lexical
                        // indexing. The vectors travel with the text so the
                        // semantic tier never has to touch the file again.
                        let (units, content_hash) = match (model.as_ref(), content.as_deref()) {
                            (Some(model), Some(text)) if !text.trim().is_empty() => {
                                let chunks = chunk_text(text, job.size as usize);
                                let vectors: Vec<(usize, usize, Vec<i8>)> = chunks
                                    .into_par_iter()
                                    .map(|(start, end, chunk)| {
                                        (start, end, model.embed_to_i8(&chunk))
                                    })
                                    .collect();
                                (vectors, Some(format!("{:x}", fnv_hash(text))))
                            }
                            _ => (Vec::new(), None),
                        };

                        let done = extract_done.fetch_add(1, Ordering::Relaxed) + 1;
                        if done.is_multiple_of(5) || done == extract_total {
                            let mut st = producer_status.write();
                            st.extract_done = done;
                            st.current_status =
                                format!("Extracting text… {}/{} files", done, extract_total);
                            Self::emit_status(&producer_app, &st);
                        }

                        // Consumer may be gone only if the crawl is shutting
                        // down; dropping the work is safe in that case.
                        let _ = tx.send(ExtractedWork {
                            node_id: job.node_id,
                            path: job.path.clone(),
                            name: job.name.clone(),
                            modified: job.modified,
                            size: job.size,
                            content,
                            units,
                            content_hash,
                            image_sig,
                            image_phash,
                            phash_needs_refresh: job.phash_needs_refresh,
                            retry,
                        });
                    });
                });

                // Single writer for both indexes (Tantivy requires one).
                let index_guard = content_index.read();
                let c_index = index_guard.as_ref();
                for work in rx {
                    if let Some(ref content) = work.content
                        && let Some(c_index) = c_index
                    {
                        let _ = c_index.add_or_update(
                            &work.path,
                            &work.name,
                            content,
                            work.modified,
                            work.size,
                            work.path.extension().and_then(|ext| ext.to_str()),
                        );
                        content_count += 1;
                        pending_commits += 1;

                        // Publish live counts instead of waiting for a commit
                        // boundary, otherwise the UI reports 0 documents while
                        // extraction is still running.
                        if content_count.is_multiple_of(64) {
                            let mut st = status.write();
                            st.files_indexed = files_count;
                            st.content_indexed = content_count;
                            Self::emit_status(&app_handle, &st);
                        }
                    }

                    if let Some(signature) = work.image_sig.as_deref()
                        && let Some(db) = db_handle.as_ref()
                    {
                        if let Ok(updates) = db.set_image_text_sig(&work.path, signature) {
                            if !updates.is_empty() {
                                metadata.write().apply_bursts(&updates);
                            }
                            if let Some(phash) = work.image_phash
                                && phash != u64::MAX
                            {
                                let _ = db.set_image_phash(&work.path, phash);
                                match db.assign_burst_for_path(&work.path) {
                                    Ok(updates) if !updates.is_empty() => {
                                        metadata.write().apply_bursts(&updates);
                                    }
                                    _ => metadata.write().clear_burst_for_path(&work.path),
                                }
                            } else if work.phash_needs_refresh {
                                if let Ok(updates) = db.clear_image_hash(&work.path)
                                    && !updates.is_empty()
                                {
                                    metadata.write().apply_bursts(&updates);
                                }
                                metadata.write().clear_burst_for_path(&work.path);
                            } else {
                                // Pending OCR: the pixels did not change, so
                                // keep the existing dHash for this re-assignment.
                                match db.assign_burst_for_path(&work.path) {
                                    Ok(updates) if !updates.is_empty() => {
                                        metadata.write().apply_bursts(&updates);
                                    }
                                    _ => metadata.write().clear_burst_for_path(&work.path),
                                }
                            }
                        } else {
                            metadata.write().clear_burst_for_path(&work.path);
                        }
                    }

                    if work.retry {
                        retry_paths.insert(work.path.clone());
                    } else if pending_images.contains(&work.path) && work.content.is_some() {
                        extracted_paths.insert(work.path.clone());
                    }

                    if let Some(model_id) = embed_model_id
                        && let Some(db) = db_handle.as_ref()
                    {
                        for (start, end, vector) in &work.units {
                            if let Ok(id) =
                                db.insert_unit(work.node_id, *start, *end, model_id, vector)
                            {
                                let unit = Unit {
                                    id,
                                    node_id: work.node_id,
                                    byte_start: *start,
                                    byte_end: *end,
                                    model_id,
                                };
                                unit_store.write().add_unit(unit, vector);
                                units_added += 1;
                                if units_added.is_multiple_of(256) {
                                    let mut st = status.write();
                                    st.units_indexed = unit_store.read().len();
                                    st.semantic_ready = st.units_indexed > 0;
                                    Self::emit_status(&app_handle, &st);
                                }
                            }
                        }
                        let _ = db.update_node_indexed_gen(
                            work.node_id,
                            1,
                            work.content_hash.as_deref(),
                        );
                    }

                    if pending_commits >= 512
                        && let Some(c_index) = c_index
                    {
                        let _ = c_index.commit();
                        pending_commits = 0;
                        let mut st = status.write();
                        st.content_indexed = content_count;
                        Self::emit_status(&app_handle, &st);
                    }
                }
            });

            {
                let mut st = status.write();
                st.extract_done = extract_total;
                st.content_indexed = content_count;
                if semantic_enabled {
                    st.units_indexed = unit_store.read().len();
                    st.semantic_ready = st.units_indexed > 0;
                }
                st.phase = "committing".to_string();
                st.current_status = "Updating search index...".to_string();
                Self::emit_status(&app_handle, &st);
            }

            // A pending image re-scanned by this crawl is done. When extraction
            // produced nothing because OCR is off, drop any stale OCR document
            // so search stops returning it. Paths skipped by exclusions never
            // enter `pending_images`, so their flag is kept.
            //
            // Incomplete reads are the exception: their old text stays (it is
            // better than nothing) and they stay pending for the next crawl.
            let ocr_on = crate::indexer::extract::ocr_enabled();
            for path in &pending_images {
                if retry_paths.contains(path) {
                    continue;
                }
                if !extracted_paths.contains(path)
                    && !ocr_on
                    && let Some(c_index) = content_index.read().as_ref()
                {
                    let _ = c_index.remove(path);
                }
                metadata.write().clear_content_pending(path);
            }
            // Retry, but a bounded number of times. A file that always times
            // out used to be re-read on every single launch, so the crawl never
            // settled and launching the app meant waiting for the same
            // expensive files again. After the cap the file is left alone
            // until it changes on disk or OCR settings change, which is what
            // resets the counter.
            let mut given_up = 0usize;
            for path in &retry_paths {
                let keep_trying = metadata
                    .read()
                    .db()
                    .map(|db| db.record_retry(path, MAX_EXTRACTION_ATTEMPTS).unwrap_or(true))
                    .unwrap_or(true);
                if keep_trying {
                    metadata.write().set_content_pending(path);
                } else {
                    given_up += 1;
                }
            }
            if !retry_paths.is_empty() {
                crate::say!(
                    "[Kensaku Indexer] {} file(s) left pending for retry (timeout or busy){}",
                    retry_paths.len() - given_up,
                    if given_up > 0 {
                        format!(", {given_up} given up on after {MAX_EXTRACTION_ATTEMPTS} attempts")
                    } else {
                        String::new()
                    }
                );
            }

            // Commit the SQLite transaction (batched writes)
            if let Some(db) = metadata.read().db() {
                let _ = db.commit_transaction();
                // Keep the WAL from growing across crawls. PASSIVE never
                // blocks readers/writers, so this is safe on the crawl thread.
                db.checkpoint_passive();
            }

            // A configured root that cannot be read (unplugged external drive,
            // unmounted network share) is not "everything was deleted": keep
            // its metadata and documents so reconnecting the drive does not
            // require a full rebuild.
            let unavailable_roots: Vec<PathBuf> = watched_dirs
                .iter()
                .filter(|root| std::fs::read_dir(root).is_err())
                .cloned()
                .collect();
            let belongs_to_unavailable_root = |path: &std::path::Path| {
                unavailable_roots.iter().any(|root| path.starts_with(root))
            };

            if !unavailable_roots.is_empty() {
                let hint = format!(
                    "{} roots unavailable, index preserved",
                    unavailable_roots.len()
                );
                crate::say!("[Kensaku Indexer] {}: {:?}", hint, unavailable_roots);
                let mut st = status.write();
                st.current_status = hint;
                Self::emit_status(&app_handle, &st);
            }

            // A directory the crawl could not read (permissions, a locked
            // file, a network hiccup, a Windows path over MAX_PATH) is not
            // "its contents were deleted". Only whole roots were protected, so
            // one unreadable subfolder dropped its entire subtree from the
            // index and recovering it needed a full re-extraction.
            //
            // Cached per directory: the sweep asks about every stale
            // candidate, and a deleted tree shares one parent.
            let mut parent_unreadable: HashMap<PathBuf, bool> = HashMap::new();
            let mut is_protected_by_unreadable_parent = |path: &std::path::Path| -> bool {
                let Some(parent) = path.parent() else {
                    return false;
                };
                if let Some(known) = parent_unreadable.get(parent) {
                    return *known;
                }
                // NotFound means the parent really is gone, so the child is
                // really gone. Any other error means we could not look.
                let unreadable = match std::fs::read_dir(parent) {
                    Ok(_) => false,
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                };
                parent_unreadable.insert(parent.to_path_buf(), unreadable);
                unreadable
            };

            // Drop metadata entries for files that no longer exist on disk.
            let stale_paths: Vec<PathBuf> = {
                let store = metadata.read();
                (0..store.len())
                    .filter_map(|i| {
                        let e = store.materialize(i as u32);
                        if crawled_paths.contains(&e.path)
                            || belongs_to_unavailable_root(&e.path)
                            || is_protected_by_unreadable_parent(&e.path)
                        {
                            return None;
                        }
                        Some(e.path)
                    })
                    .collect()
            };

            if !stale_paths.is_empty() {
                // Drop content documents for the same paths directly. The old
                // implementation scanned the entire content index on every
                // crawl, which cost seconds on large corpora; the crawl already
                // knows exactly what disappeared.
                if let Some(c_index) = content_index.read().as_ref() {
                    let _ = c_index.remove_many(&stale_paths);
                }

                let mut store = metadata.write();
                for path in &stale_paths {
                    store.remove(path);
                }
                crate::say!(
                    "[Kensaku Indexer] Removed {} stale metadata entries",
                    stale_paths.len()
                );
            }

            // Final commit
            if let Some(c_index) = content_index.read().as_ref() {
                let _ = c_index.commit();
            }

            last_reconcile_unix_secs.store(now_unix_secs(), Ordering::Relaxed);

            // Record what this crawl actually did, so "indexing runs every
            // time I open the app" can be diagnosed from the Health tab
            // instead of guessed at. Reasons are checked in the order they are
            // set below the crawl, widest first: a schema rebuild re-reads
            // everything, so it explains the rest.
            {
                let rebuild_reason = content_index
                    .read()
                    .as_ref()
                    .and_then(|index| index.rebuild_reason.clone());
                // Every contribution is named, not just the first one found.
                // The old form picked the first matching branch, so a scan
                // that re-read 93,390 of 100,102 entries reported
                // "OCR settings changed (0 image/PDF pending)": a cause that
                // accounted for zero files, presented as the reason for all of
                // them. A reason nobody can check is worse than no reason.
                let mut causes: Vec<String> = Vec::new();
                if let Some(reason) = rebuild_reason {
                    causes.push(format!("content index rebuilt: {reason}"));
                }
                if ocr_queued > 0 {
                    causes.push(format!(
                        "OCR settings changed, {ocr_queued} image/PDF queued"
                    ));
                }
                if pending_files > 0 || given_up > 0 || !retry_paths.is_empty() {
                    causes.push(format!(
                        "{pending_files} left pending by an earlier scan, {} retried, {given_up} given up on",
                        retry_paths.len().saturating_sub(given_up)
                    ));
                }
                // A genuine edit and an unstable measurement look identical
                // here, so name the counts rather than asserting a cause.
                if new_files > 0 || restamped_files > 0 {
                    causes.push(format!(
                        "{new_files} new, {restamped_files} with a different size or modification time"
                    ));
                }
                let reason = if causes.is_empty() {
                    "nothing changed".to_string()
                } else {
                    causes.join("; ")
                };

                crate::say!(
                    "[Kensaku Indexer] scan: {} entries, {} re-read: {}",
                    files_count,
                    content_count,
                    reason
                );

                write_crawl_report(&CrawlReport {
                    at: now_unix_secs(),
                    walked: files_count,
                    reindexed: content_count,
                    new_files,
                    retried: retry_paths.len().saturating_sub(given_up),
                    abandoned: given_up,
                    reason,
                    // The value from before this crawl's sync, so a change is
                    // visible instead of showing "no change" forever.
                    ocr_signature_stored: ocr_signature_before.clone(),
                    ocr_signature_now: crate::ocr::signature(),
                });
            }

            // First-scan completion gate: the first full crawl marks setup done.
            {
                let mut cfg = config_arc.write();
                if cfg.is_setup_complete && !cfg.first_scan_done {
                    cfg.first_scan_done = true;
                    let _ = cfg.save();
                }
            }

            {
                let mut st = status.write();
                st.is_indexing = false;
                st.paused = false;
                st.phase = "idle".to_string();
                st.files_indexed = files_count;
                st.content_indexed = content_count;
                st.current_status = format!("Ready ({} files indexed)", files_count);
                Self::emit_status(&app_handle, &st);
            }

            crate::say!(
                "[Kensaku Indexer] Finished initial index: {} files ({} with full text)",
                files_count, content_count
            );

            // Start file watcher for live changes, replacing any previous one.
            let handle = start_file_watcher(
                watched_dirs,
                metadata.clone(),
                content_index.clone(),
                config_arc.clone(),
                Arc::clone(&watcher_ok),
            );
            *watcher_handle.lock() = Some(handle);

            // Start background embedding worker if semantic search is enabled
            if config_arc.read().semantic_enabled {
                let mut eq_guard = embedding_queue.lock();
                if eq_guard.is_none() {
                    let model = ModelRegistry::global().clone();
                    let mut q = EmbeddingQueue::new(
                        model,
                        Arc::clone(&unit_store),
                        Some(Arc::clone(&status)),
                        Some(Arc::clone(&app_handle)),
                    );
                    q.query_in_flight = Arc::clone(&query_in_flight);
                    if let Some(db) = metadata.read().db() {
                        q.start(db);
                    }
                    *eq_guard = Some(q);
                } else if let Some(ref mut q) = *eq_guard
                    && let Some(db) = metadata.read().db()
                {
                    q.start(db);
                }
            }

            is_running.store(false, Ordering::SeqCst);
            gate_guard.armed = false;

            // Fill in perceptual hashes that the crawl skipped, one image at a
            // time. Safe to start here: the crawl transaction is committed and
            // the worker pauses if another crawl begins.
            start_phash_backfill(
                Arc::clone(&metadata),
                Arc::clone(&content_index),
                Arc::clone(&visual_store),
                Arc::clone(&is_running),
                Arc::clone(&query_in_flight),
            );
        });
    }

    /// Rebuilds the index in place and starts a fresh crawl, with no process
    /// restart. The stores behind `Arc<RwLock<_>>` are emptied and swapped
    /// live, the Tantivy writer is dropped with `content_index`, and the
    /// normal crawl repopulates everything with its usual status events.
    ///
    /// The previous implementation wrote a marker and called
    /// `AppHandle::restart()`, which spawned a second process while this one
    /// still held the single-instance lock: on losing the race the fresh
    /// instance exited and the old one was gone (the app closed and would not
    /// open again), the marker consumer deleted `index/` and `metadata.db`
    /// under the still-running old process, and re-spawning the binary
    /// flashed a console window in dev builds.
    pub fn rebuild_all(self: Arc<Self>) -> Result<(), String> {
        // The rebuild gate is the crawl gate: reject instead of racing a crawl
        // that is already running.
        if self
            .is_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(
                "Indexing is already running — wait for the scan to finish, then rebuild."
                    .to_string(),
            );
        }

        let mut st = self.status.write();
        st.is_indexing = true;
        st.paused = false;
        st.phase = "rebuilding".to_string();
        st.files_indexed = 0;
        st.total_files = 0;
        st.extract_done = 0;
        st.extract_total = 0;
        st.current_status = "Rebuilding index…".to_string();
        Self::emit_status(&self.app_handle, &st);
        drop(st);

        thread::spawn(move || self.run_rebuild());
        Ok(())
    }

    /// Teardown half of `rebuild_all`: runs on its own thread because stopping
    /// the embedding worker joins it, then hands the empty stores to the
    /// normal crawl for repopulation.
    fn run_rebuild(self: Arc<Self>) {
        // A panic anywhere below would leave the crawl gate (`is_running`)
        // stuck, blocking every later crawl and rebuild for the process
        // lifetime. The guard hands the gate back even on an unwind; the
        // paths that finish on purpose settle it first and release it
        // themselves.
        struct GateGuard {
            gate: Arc<AtomicBool>,
            status: Arc<RwLock<IndexingStatus>>,
            app_handle: Arc<RwLock<Option<tauri::AppHandle>>>,
            settled: bool,
        }
        impl Drop for GateGuard {
            fn drop(&mut self) {
                if !self.settled {
                    let mut st = self.status.write();
                    st.is_indexing = false;
                    st.paused = false;
                    st.phase = "idle".to_string();
                    st.current_status = "Rebuild failed".to_string();
                    IndexerManager::emit_status(&self.app_handle, &st);
                }
                self.gate.store(false, Ordering::SeqCst);
            }
        }
        let mut guard = GateGuard {
            gate: Arc::clone(&self.is_running),
            status: Arc::clone(&self.status),
            app_handle: Arc::clone(&self.app_handle),
            settled: false,
        };

        // Freeze every writer that touches the stores about to be wiped.
        // Holding the crawl gate pauses the phash backfill and blocks new
        // crawls; the watcher and the embedding worker are stopped here.
        if let Some(mut handle) = self.watcher_handle.lock().take() {
            handle.stop();
        }
        self.watcher_ok.store(false, Ordering::SeqCst);
        if let Some(mut queue) = self.embedding_queue.lock().take() {
            queue.stop();
        }

        {
            let mut st = self.status.write();
            st.current_status = "Rebuilding index — clearing old data…".to_string();
            Self::emit_status(&self.app_handle, &st);
        }

        // Clearing `content_index` drops the Tantivy writer and reader, so
        // nothing keeps the index directory locked afterwards (Windows cares).
        *self.content_index.write() = None;

        // Wipe the database through the live connection, then swap in a fresh
        // in-memory store that reopens it empty. The wipe is plain SQL, so no
        // open file handle anywhere can break it — unlike the delete-the-file
        // approach of the old restart-based rebuild.
        if let Some(db) = self.metadata.read().db() {
            let _ = db.reset_visual_index();
            let _ = db.clear();
        }
        *self.metadata.write() = MetadataStore::new();
        self.unit_store.write().clear();
        self.visual_store.write().clear();

        let index_dir = KensakuConfig::index_dir();
        // Deleting the directory reclaims the old segments outright. If a
        // just-dropped mmap still holds a file (Windows), the fresh handle
        // opens the stale index and `truncate` purges it logically instead.
        let reclaimed = std::fs::remove_dir_all(&index_dir).is_ok();
        let mut fresh_index = match ContentIndex::new(&index_dir) {
            Ok(index) => Some(index),
            Err(e) => {
                crate::say_err!(
                    "[Kensaku] Rebuild failed: could not recreate the content index: {}",
                    e
                );
                let mut st = self.status.write();
                st.is_indexing = false;
                st.phase = "error".to_string();
                st.current_status = format!("Rebuild failed: {}", e);
                Self::emit_status(&self.app_handle, &st);
                // The detailed status above is final for this run; the guard
                // only needs to hand the crawl gate back.
                guard.settled = true;
                drop(guard);
                return;
            }
        };
        if !reclaimed
            && let Err(e) = fresh_index
                .as_mut()
                .expect("fresh index exists on the Ok path")
                .truncate()
        {
            // Purge failed too: the crawl below still overwrites documents
            // for existing paths, but stale ones linger until a successful
            // rebuild. Loud enough to notice.
            crate::say_err!(
                "[Kensaku] Rebuild: could not purge the old content index: {}",
                e
            );
        }
        *self.content_index.write() = fresh_index;

        // Cached extractions would otherwise be served back into the fresh
        // index instead of re-reading the files.
        crate::indexer::extract::clear_cache();

        // Thumbnails are derived data and regenerate during the crawl.
        // Best-effort: files still open for a read are simply skipped.
        let _ = std::fs::remove_dir_all(KensakuConfig::config_dir().join("thumbs"));

        // Hand the empty stores to the normal crawl. Settle first so the
        // guard touches no status, release the gate explicitly (the crawl
        // must be able to take it again), then start: `start_indexing`
        // re-takes the gate, emits its own walking status, and restarts the
        // watcher and the embedding worker when done.
        guard.settled = true;
        drop(guard);
        self.start_indexing();
    }

    pub fn get_status(&self) -> IndexingStatus {
        self.status.read().clone()
    }

    /// Runs a query against the live, in-process stores (no second open, no
    /// per-query metadata/vector reload). Used by the daemon search method.
    pub fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
        with_snippets: bool,
    ) -> Result<SearchResponse, String> {
        let config = self.config.read();
        let aliases = config.alias_pairs();
        let metadata = self.metadata.read();
        let content = self.content_index.read();
        let units = self.unit_store.read();
        let visual = self.visual_store.read();
        Ok(
            SearchDispatcher::search_response_fast_sorted_with_snippets_and_visual(
                query,
                &metadata,
                &content,
                Some(&units),
                Some(&visual),
                config.semantic_enabled,
                limit,
                0,
                SortOrder::Best,
                &aliases,
                offset,
                with_snippets,
            ),
        )
    }

    /// Keyset-paginated exhaustive search for agent surfaces: returns the
    /// page of matches strictly after `start_after` (a path cursor), so
    /// result sets beyond the 10k exhaustive cap stay reachable without
    /// materialising more than one window.
    pub fn search_page_after(
        &self,
        query: &str,
        limit: usize,
        start_after: &str,
        with_snippets: bool,
    ) -> Result<SearchResponse, String> {
        let config = self.config.read();
        let aliases = config.alias_pairs();
        let metadata = self.metadata.read();
        let content = self.content_index.read();
        let units = self.unit_store.read();
        Ok(SearchDispatcher::search_response_exhaustive_after(
            query,
            &metadata,
            &content,
            Some(&units),
            config.semantic_enabled,
            limit,
            0,
            &aliases,
            start_after,
            with_snippets,
        ))
    }

    /// Looks up visually similar images for a given path.
    pub fn find_similar_images(
        &self,
        path: &std::path::Path,
        limit: usize,
    ) -> Result<Vec<crate::embed::visual::SimilarImageResult>, String> {
        let metadata = self.metadata.read();
        let visual_store = self.visual_store.read();

        // 1. Resolve source entry and vector
        let (source_node_id, query_vec) = if let Some(entry) = metadata.get(path) {
            let vec_opt = visual_store.get_vector(entry.id).map(|v| v.to_vec());
            (Some(entry.id), vec_opt)
        } else {
            (None, None)
        };

        let query_vec = match query_vec {
            Some(v) => v,
            None => {
                let encoder = crate::embed::visual::active_encoder().ok_or_else(|| {
                    "Visual embeddings are not available in this build; install the visual model pack"
                        .to_string()
                })?;
                let thumbs_dir = KensakuConfig::config_dir().join("thumbs");
                let thumb = crate::indexer::extract::image::thumbnail_path(path, &thumbs_dir);
                if thumb.is_file()
                    && let Some(v) = encoder.embed_image_from_thumbnail(&thumb)
                {
                    v
                } else if path.is_file() {
                    // Budgeted decode, same as OCR: the page is charged to the
                    // decode budget until embedding has consumed it.
                    let (img, _reserved) =
                        crate::indexer::extract::image::decode_for_recognition(path)
                            .ok_or_else(|| {
                                format!("Failed to open image {}", path.display())
                            })?;
                    encoder.embed_image(&img)
                } else {
                    return Err(format!(
                        "Image not found or not readable: {}",
                        path.display()
                    ));
                }
            }
        };

        let floor = 0.30f32;
        let hits = visual_store.find_similar(&query_vec, source_node_id, floor, limit);

        let mut results = Vec::new();
        for (node_id, similarity) in hits {
            if let Some(entry) = metadata.get_by_node_id(node_id) {
                let is_image = entry
                    .path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(crate::indexer::extract::image::is_supported_image_extension);
                if is_image {
                    results.push(crate::embed::visual::SimilarImageResult {
                        path: entry.path.to_string_lossy().to_string(),
                        filename: entry.name,
                        similarity,
                        file_size: entry.size,
                        modified: entry.modified,
                    });
                }
            }
        }

        Ok(results)
    }

    /// Compact live state shared by the daemon, CLI and MCP. Avoids loading
    /// the embedding table or re-opening SQLite/Tantivy.
    pub fn state_json(&self) -> Result<serde_json::Value, String> {
        let (files, dirs) = {
            let store = self.metadata.read();
            let mut files = 0u64;
            let mut dirs = 0u64;
            for index in 0..store.len() {
                if store.node(index as u32).is_dir() {
                    dirs += 1;
                } else {
                    files += 1;
                }
            }
            (files, dirs)
        };
        let content_docs = self
            .content_index
            .read()
            .as_ref()
            .map(|index| index.num_docs())
            .unwrap_or(0);
        let semantic_units = self.unit_store.read().len() as u64;
        let config = self.config.read();
        let roots: Vec<String> = config
            .watched_dirs
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let units_model_id = match self.metadata.read().db() {
            Some(db) => db.units_model_id()?,
            None => None,
        };
        let model_dir = KensakuConfig::config_dir().join("models");
        let real_model = model_dir.join("model.safetensors").is_file()
            && model_dir.join("tokenizer.json").is_file();
        let (active_model_id, model_name) = if real_model {
            (
                crate::embed::model::REAL_MODEL_ID,
                "potion-base-8M".to_string(),
            )
        } else {
            (
                crate::embed::model::DEFAULT_MODEL_ID,
                "static-embed-256".to_string(),
            )
        };
        let index_dir = KensakuConfig::index_dir();
        let index_bytes = dir_size_bytes(&index_dir).saturating_add(prefixed_file_bytes(
            &KensakuConfig::config_dir(),
            "metadata.db",
        ));
        let last_reconcile = last_modified_secs(&[
            KensakuConfig::config_dir().join("metadata.db"),
            index_dir.join("meta.json"),
        ]);
        let status = self.get_status();

        Ok(serde_json::json!({
            "files": files,
            "dirs": dirs,
            "content_docs": content_docs,
            "content_index_ready": self.content_index.read().is_some(),
            "semantic_units": semantic_units,
            "index_bytes": index_bytes,
            "model_id": units_model_id,
            "active_model_id": active_model_id,
            "model_name": model_name,
            "model_real": real_model,
            "roots": roots,
            "last_reconcile": last_reconcile,
            "is_indexing": status.is_indexing,
            "paused": status.paused,
            "phase": status.phase,
            "files_indexed": status.files_indexed,
            "content_indexed": status.content_indexed,
            "extract_done": status.extract_done,
            "extract_total": status.extract_total,
        }))
    }

    /// True while a live filesystem watcher is running.
    pub fn watcher_ok(&self) -> bool {
        self.watcher_ok.load(Ordering::Relaxed)
    }

    /// True while a crawl is in progress.
    ///
    /// The watcher is stopped for the duration of a crawl and restarted when
    /// it finishes, so "watcher not running" is the normal state during a scan
    /// and must not be reported as a fault.
    pub fn is_crawling(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    /// Brings the process OCR state in line with config and the machine.
    ///
    /// - Applies `ocr_enabled` / `ocr_engine`.
    /// - When Tesseract cannot run and PP-OCR is compiled in, switches to
    ///   PP-OCR (the common case on a fresh Windows or Mac) and fetches its
    ///   pack. This used to happen in the GUI only, bypassing every re-queue,
    ///   so images indexed before the switch never got text.
    /// - Compares [`crate::ocr::signature`] with the one recorded for this
    ///   index. On a change (an engine became usable, the user switched, an
    ///   OCR fix shipped) every image and PDF is re-queued and the extraction
    ///   cache dropped.
    ///
    /// Returns how many images/PDFs were queued for a re-read. Zero is a
    /// meaningful answer: the signature changed but nothing was left to
    /// re-read, and a crawl reason must not blame OCR for work it did not
    /// cause.
    fn sync_ocr_state(
        config_arc: &Arc<RwLock<KensakuConfig>>,
        metadata: &Arc<RwLock<MetadataStore>>,
    ) -> usize {
        let mut config = config_arc.read().clone();
        crate::indexer::extract::image::reset_tesseract_probe();
        crate::indexer::extract::set_ocr_enabled(config.ocr_enabled);
        crate::ocr::set_engine(&config.ocr_engine);
        if !config.ocr_enabled {
            return 0;
        }

        if crate::ocr::accurate_compiled() {
            if crate::ocr::engine_name() == crate::ocr::TESSERACT
                && !crate::indexer::extract::image::tesseract_usable()
            {
                config.ocr_engine = crate::ocr::ACCURATE.to_string();
                crate::ocr::set_engine(crate::ocr::ACCURATE);
                match config.save() {
                    Ok(()) => {
                        crate::say_err!("[Kensaku OCR] tesseract not found; switched to PP-OCR");
                        *config_arc.write() = config.clone();
                    }
                    Err(e) => crate::say_err!("[Kensaku OCR] could not save the engine switch: {e}"),
                }
            }
            if crate::ocr::engine_name() == crate::ocr::ACCURATE && !crate::ocr::pack_present() {
                let _ = crate::models::download_ocr_pack();
            }
            if crate::ocr::engine_name() == crate::ocr::INDRA && !crate::ocr_indra::pack_present() {
                let _ = crate::models::download_indra_pack();
            }
        }

        // Never downgrade a stored signature to "no engine".
        //
        // The engine probe runs once per process and its failure is cached,
        // and at startup it can fail for reasons that have nothing to do with
        // this machine: the ONNX runtime library being scanned by antivirus,
        // or a self-test losing a race with the engine pool loading. Writing
        // "none" in that state meant the *next* launch saw a different
        // signature and queued every image and scanned PDF for re-reading —
        // a whole-corpus OCR pass triggered by a transient failure.
        //
        // Leaving the stored signature alone when nothing can read images
        // keeps the "engine became usable, re-read the images" path intact
        // for a first run (where nothing is stored yet) without letting a
        // temporary failure re-read anything.
        let signature = crate::ocr::signature();
        let stored = read_ocr_signature();
        if stored.as_deref() == Some(signature.as_str()) {
            return 0;
        }
        crate::say_err!(
            "[Kensaku OCR] state changed: was {:?}, now {:?}",
            stored.as_deref().unwrap_or("<none recorded>"),
            signature
        );
        if crate::ocr::usable_engine() == crate::ocr::NONE {
            crate::say_err!(
                "[Kensaku OCR] no OCR engine can run right now; keeping the recorded state so a \
                 temporary failure does not re-read every image"
            );
            return 0;
        }
        write_ocr_signature(&signature);
        crate::indexer::extract::clear_cache();
        let marked = metadata.write().mark_ocr_targets_pending();
        crate::say_err!(
            "[Kensaku OCR] using {} (revision {}); {} image/PDF file(s) queued for OCR",
            crate::ocr::usable_engine(),
            crate::ocr::OCR_REVISION,
            marked
        );
        marked
    }

    /// Starts a crawl now, or as soon as the running one finishes. Used when
    /// the OCR pack lands mid-crawl: a plain `start_indexing` would be a
    /// no-op and the newly OCR-able images would wait for the next restart.
    ///
    /// Never crawls before the setup wizard is done: a pack download can
    /// finish while the user has not chosen their folders yet, and the
    /// config then still holds the default roots (the home directory), so
    /// the wrong thing gets scanned and watched.
    /// Clears semantic units that belong to a different embedding model, then
    /// starts a crawl so they are rebuilt with the active one.
    ///
    /// The same reset runs at construction, which covered the restart case. It
    /// did not cover downloading the model files while the app is running: the
    /// units kept hashing vectors while queries used the real model, and two
    /// different vector spaces compared against each other produce confident
    /// nonsense. The registry now reloads on download and calls this.
    pub fn on_semantic_model_changed(self: &Arc<Self>) {
        let expected_model_id = crate::embed::ModelRegistry::global().model_id;
        if let Some(db) = self.metadata.read().db() {
            match db.units_model_id() {
                Ok(Some(existing)) if existing != expected_model_id => {
                    match db.reset_semantic_index() {
                        Ok(()) => {
                            self.unit_store.write().clear();
                            crate::say_err!(
                                "[Kensaku] Semantic model changed (units id {existing} -> {expected_model_id}); cleared semantic index for a rebuild"
                            );
                        }
                        Err(e) => crate::say_err!("[Kensaku] Failed to reset semantic index: {}", e),
                    }
                }
                Ok(_) => {}
                Err(e) => crate::say_err!("[Kensaku] Failed to read semantic model id: {}", e),
            }
        }
        self.start_indexing_when_idle();
    }

    pub fn start_indexing_when_idle(self: &Arc<Self>) {
        if !self.config.read().is_setup_complete {
            return;
        }
        let manager = Arc::clone(self);
        let _ = thread::Builder::new()
            .name("kensaku-crawl-when-idle".into())
            .spawn(move || {
                while manager.is_running.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_secs(2));
                }
                manager.start_indexing();
            });
    }

    pub fn apply_config(&self, config: KensakuConfig) -> Result<(), String> {
        let previous = self.config.read().clone();
        let ocr_changed =
            previous.ocr_enabled != config.ocr_enabled || previous.ocr_engine != config.ocr_engine;
        let extractors_changed = previous.extractors != config.extractors;
        // Raising a content cap only helps files that were cut off before, and
        // those are unchanged on disk, so they need an explicit re-read.
        let caps_raised = config.content_max_file_size_mb > previous.content_max_file_size_mb
            || config.content_max_text_bytes > previous.content_max_text_bytes
            || config.content_max_semantic_units > previous.content_max_semantic_units;

        config.save()?;
        *self.config.write() = config.clone();
        crate::util::priority::apply(config.crawl_mode);
        crate::indexer::extract::image::set_ocr_max_image_bytes(config.ocr_max_image_bytes);
        crate::indexer::extract::set_content_caps(config.content_max_file_size_mb, config.content_max_text_bytes);
        crate::embed::set_max_semantic_units(config.content_max_semantic_units);

        let mut reindex = false;
        if caps_raised {
            crate::indexer::extract::clear_cache();
            let marked = self.metadata.write().mark_all_content_pending();
            crate::say_err!("[Kensaku] Content caps raised: {marked} file(s) queued for re-read");
            reindex = true;
        }
        if ocr_changed {
            crate::indexer::extract::set_ocr_enabled(config.ocr_enabled);
            crate::ocr::set_engine(&config.ocr_engine);
            crate::indexer::extract::clear_cache();
            let marked = self.metadata.write().mark_ocr_targets_pending();
            crate::say_err!(
                "[Kensaku] OCR {}: {} image(s) queued for re-scan",
                if config.ocr_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                marked
            );
            reindex = true;
        }

        if extractors_changed {
            crate::indexer::extract::set_external_extractors(config.extractors.clone());
            let active: Vec<_> = previous
                .extractors
                .iter()
                .chain(config.extractors.iter())
                .filter(|extractor| extractor.enabled)
                .collect();
            if !active.is_empty() {
                let extensions: Vec<String> = active
                    .iter()
                    .flat_map(|extractor| extractor.extensions.iter().cloned())
                    .collect();
                let broad = active.iter().any(|extractor| {
                    extractor
                        .mime
                        .iter()
                        .any(|selector| !selector.trim().is_empty())
                        || extractor
                            .extensions
                            .iter()
                            .any(|selector| selector.trim().trim_start_matches('.') == "*")
                });
                let marked = if broad || extensions.is_empty() {
                    self.metadata.write().mark_all_content_pending()
                } else {
                    self.metadata.write().mark_extensions_pending(&extensions)
                };
                crate::say_err!(
                    "[Kensaku] External extractors changed; {} file(s) queued for re-scan",
                    marked
                );
                reindex = true;
            }
        }

        // An OCR engine became usable without a config change (pack
        // downloaded by the GUI while a daemon owns the index): the crawl's
        // `sync_ocr_state` notices and re-queues.
        if !reindex
            && config.ocr_enabled
            && read_ocr_signature().as_deref() != Some(crate::ocr::signature().as_str())
        {
            reindex = true;
        }

        if reindex {
            self.start_indexing();
        }
        Ok(())
    }
}

/// Where the OCR signature of the current index is recorded.
/// What the last crawl actually did.
///
/// Written at the end of every crawl so the reason a launch re-read files is
/// visible after the fact. Without it, "indexing runs on every start" cannot
/// be told apart from an ordinary reconcile: the diagnostics only went to
/// stderr, which a packaged build never shows anyone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlReport {
    pub at: u64,
    /// Files and folders the walk visited.
    pub walked: usize,
    /// Files whose content was extracted (i.e. actually re-read).
    pub reindexed: usize,
    /// Files that were absent from the content index before this crawl.
    ///
    /// A first scan, or an index that was recreated, re-reads every file
    /// while reporting it here, so the doctor can tell an expected full scan
    /// apart from churn over files it had already read.
    #[serde(default)]
    pub new_files: usize,
    /// Files left pending for a retry after this crawl.
    pub retried: usize,
    /// Files dropped from the retry loop (retry budget exhausted).
    pub abandoned: usize,
    /// Why anything was reindexed. "nothing changed" when nothing was.
    pub reason: String,
    /// OCR state as recorded, and as computed by this launch. Equal values mean
    /// no image should have been re-read for OCR.
    #[serde(default)]
    pub ocr_signature_stored: Option<String>,
    #[serde(default)]
    pub ocr_signature_now: String,
}

fn crawl_report_path() -> std::path::PathBuf {
    KensakuConfig::config_dir().join("crawl-last.json")
}

fn write_crawl_report(report: &CrawlReport) {
    match serde_json::to_string_pretty(report) {
        Ok(json) => {
            if let Err(e) = std::fs::write(crawl_report_path(), json) {
                crate::say_err!("[Kensaku] could not record the crawl report: {e}");
            }
        }
        Err(e) => crate::say_err!("[Kensaku] could not serialise the crawl report: {e}"),
    }
}

/// The last recorded crawl, for the doctor and for support.
pub fn last_crawl_report() -> Option<CrawlReport> {
    let raw = std::fs::read_to_string(crawl_report_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn ocr_signature_path() -> PathBuf {
    KensakuConfig::config_dir().join("ocr-state")
}

fn read_ocr_signature() -> Option<String> {
    std::fs::read_to_string(ocr_signature_path())
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_ocr_signature(signature: &str) {
    if let Err(e) = std::fs::write(ocr_signature_path(), signature) {
        crate::say_err!("[Kensaku OCR] could not record OCR state: {e}");
    }
}

fn dir_size_bytes(dir: &std::path::Path) -> u64 {
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
            total = total.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
        }
    }
    total
}

fn prefixed_file_bytes(dir: &std::path::Path, prefix: &str) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        if entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            total = total.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
        }
    }
    total
}

fn last_modified_secs(paths: &[PathBuf]) -> Option<u64> {
    paths
        .iter()
        .filter_map(|path| {
            std::fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(modified: u64, size: u64, content_pending: bool) -> FileEntry {
        FileEntry {
            id: 0,
            path: PathBuf::from("/tmp/kensaku/example.bin"),
            name: "example.bin".to_string(),
            extension: Some("bin"),
            size,
            modified,
            is_dir: false,
            dev: 0,
            ino: 0,
            char_bloom: 0,
            content_pending,
        }
    }

    #[test]
    fn crawl_orders_text_first_then_newest_then_smallest() {
        let job = |path: &str, modified: u64, size: u64, is_image: bool| ExtractJob {
            node_id: 0,
            path: PathBuf::from(path),
            name: path.to_string(),
            modified,
            size,
            is_image,
            phash_needs_refresh: false,
        };
        let mut jobs = [
            job("/old-image", 10, 10, true),
            job("/old-text", 10, 10, false),
            job("/new-text", 20, 50, false),
            job("/new-small-text", 20, 5, false),
        ];
        jobs.sort_unstable_by_key(extract_sort_key);
        let order: Vec<&str> = jobs.iter().map(|job| job.path.to_str().unwrap()).collect();
        assert_eq!(
            order,
            vec!["/new-small-text", "/new-text", "/old-text", "/old-image"]
        );
    }

    #[test]
    fn change_detection_honors_pending_content() {
        let existing = entry(100, 10, false);
        assert!(is_unchanged(&existing, &entry(100, 10, false)));
        assert!(!is_unchanged(&existing, &entry(101, 10, false)));
        assert!(!is_unchanged(&existing, &entry(100, 11, false)));

        let pending = entry(100, 10, true);
        assert!(
            !is_unchanged(&pending, &entry(100, 10, false)),
            "a pending entry must be re-processed even when mtime/size match"
        );
    }

    #[test]
    fn an_unstatable_file_is_not_treated_as_changed() {
        // The walker records (0, 0) when metadata() fails: a file locked by
        // another process, or one antivirus is scanning. That is a failure to
        // read the file, not a file with no content that was modified in 1970.
        let existing = entry(100, 10, false);
        assert!(
            is_unchanged(&existing, &entry(0, 0, false)),
            "an unreadable stat must not force a re-read"
        );

        // A genuinely emptied file keeps its modification time, so it is still
        // detected: size changed, time did not.
        assert!(!is_unchanged(&existing, &entry(100, 0, false)));
    }

    #[test]
    fn find_similar_images_returns_matches_and_excludes_self() {
        use crate::agent::HeadlessIndex;
        use crate::embed::visual::{MockVisualEncoder, VisualEncoder};
        use crate::indexer::metadata_db::MetadataDb;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("kensaku-sim-mgr-{}", nanos));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let db = MetadataDb::open_in_dir(&temp_dir).unwrap();
        let path1 = temp_dir.join("image_1.png");
        let path2 = temp_dir.join("image_2.png");
        let path3 = temp_dir.join("unrelated.jpg");

        let entry1 = FileEntry {
            id: 1,
            path: path1.clone(),
            name: "image_1.png".to_string(),
            extension: Some("png"),
            size: 100,
            modified: 10,
            is_dir: false,
            dev: 0,
            ino: 0,
            char_bloom: 0,
            content_pending: false,
        };
        let entry2 = FileEntry {
            id: 2,
            path: path2.clone(),
            name: "image_2.png".to_string(),
            extension: Some("png"),
            size: 100,
            modified: 10,
            is_dir: false,
            dev: 0,
            ino: 0,
            char_bloom: 0,
            content_pending: false,
        };
        let entry3 = FileEntry {
            id: 3,
            path: path3.clone(),
            name: "unrelated.jpg".to_string(),
            extension: Some("jpg"),
            size: 100,
            modified: 10,
            is_dir: false,
            dev: 0,
            ino: 0,
            char_bloom: 0,
            content_pending: false,
        };

        db.upsert(&entry1).unwrap();
        db.upsert(&entry2).unwrap();
        db.upsert(&entry3).unwrap();

        let id1 = db.get_node_id_by_path(&path1).unwrap().unwrap();
        let id2 = db.get_node_id_by_path(&path2).unwrap().unwrap();
        let id3 = db.get_node_id_by_path(&path3).unwrap().unwrap();

        let encoder = MockVisualEncoder;
        let vec_base = encoder.embed_text("same concept");
        let inverted_vec: Vec<i8> = vec_base.iter().map(|&v| -v).collect();

        db.save_visual_unit(id1, encoder.model_id(), 0x1111, &vec_base)
            .unwrap();
        db.save_visual_unit(id2, encoder.model_id(), 0x2222, &vec_base)
            .unwrap();
        db.save_visual_unit(id3, encoder.model_id(), 0x3333, &inverted_vec)
            .unwrap();

        let index = HeadlessIndex::open_at(&temp_dir).unwrap();

        let results = index
            .find_similar_images_local(path1.to_str().unwrap(), 10)
            .unwrap();

        // 1. Query path1 must be excluded
        assert!(results.iter().all(|r| r.path != path1.to_string_lossy()));

        // 2. path2 must be returned with similarity ~1.0
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, path2.to_string_lossy());
        assert!(results[0].similarity >= 0.99);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
