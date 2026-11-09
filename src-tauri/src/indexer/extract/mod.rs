pub mod delimited;
pub mod email;
pub mod external;
pub mod image;
pub mod office;
pub mod pdf;
pub mod parquet;
pub mod pdf_image;
pub mod text;

use crate::config::ExternalExtractor;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Process-wide OCR setting, mirroring [`crate::config::KensakuConfig::ocr_enabled`].
///
/// Image extraction reads this flag instead of taking a config lock, which
/// keeps the parallel extraction phase independent of the config `RwLock`.
static OCR_ENABLED: AtomicBool = AtomicBool::new(true);

/// Language codes to hand to Tesseract, mirroring
/// [`crate::config::KensakuConfig::ocr_languages`]. Empty means "pick from the
/// installed languages" (English first).
static OCR_LANGUAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Updates the process-wide OCR setting. Called whenever the config is loaded or
/// saved so image indexing follows the user's choice immediately.
pub fn set_ocr_enabled(enabled: bool) {
    OCR_ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn set_ocr_languages(languages: Vec<String>) {
    if let Ok(mut slot) = OCR_LANGUAGES.lock() {
        *slot = languages;
    }
}

pub fn ocr_languages() -> Vec<String> {
    OCR_LANGUAGES
        .lock()
        .map(|languages| languages.clone())
        .unwrap_or_default()
}

/// True when image OCR is allowed.
pub fn ocr_enabled() -> bool {
    OCR_ENABLED.load(Ordering::SeqCst)
}

/// Process-wide content caps, mirroring KensakuConfig so the extraction
/// pipeline (which runs outside the config RwLock) sees updates immediately.
static CONTENT_MAX_FILE_SIZE_BYTES: AtomicU64 = AtomicU64::new(20 * 1024 * 1024);
static CONTENT_MAX_TEXT_BYTES: AtomicUsize = AtomicUsize::new(1_000_000);

static CSV_HEAD_ROWS: AtomicUsize = AtomicUsize::new(200);

pub fn csv_head_rows() -> usize {
    CSV_HEAD_ROWS.load(Ordering::SeqCst)
}

pub fn set_content_caps(max_file_size_mb: u64, max_text_bytes: usize, csv_head_rows: usize) {
    CSV_HEAD_ROWS.store(csv_head_rows.max(1), Ordering::SeqCst);
    CONTENT_MAX_FILE_SIZE_BYTES.store(
        max_file_size_mb.saturating_mul(1024 * 1024),
        Ordering::SeqCst,
    );
    CONTENT_MAX_TEXT_BYTES.store(max_text_bytes.max(1024), Ordering::SeqCst);
    clear_extraction_cache();
}

pub fn content_max_file_size_bytes() -> u64 {
    CONTENT_MAX_FILE_SIZE_BYTES.load(Ordering::SeqCst)
}

pub fn content_max_text_bytes() -> usize {
    CONTENT_MAX_TEXT_BYTES.load(Ordering::SeqCst)
}

/// Replaces the opt-in external extractor registry and drops cached results so
/// a config change is visible immediately.
pub fn set_external_extractors(extractors: Vec<ExternalExtractor>) {
    external::set_extractors(extractors);
    clear_extraction_cache();
}

/// Drops every cached extraction. Called when OCR output may have changed
/// (engine switch, OCR revision bump), so a re-queued image is really re-read
/// instead of being served its old text from memory.
pub fn clear_cache() {
    clear_extraction_cache();
}

fn clear_extraction_cache() {
    if let Some(cache) = EXTRACTION_CACHE.get()
        && let Ok(mut cache) = cache.lock()
    {
        cache.map.clear();
        cache.order.clear();
    }
}

#[derive(Debug, Clone)]
pub struct DocumentExtract {
    pub text: String,
    pub page_count: Option<usize>,
    pub is_encrypted: bool,
    pub is_scanned: bool,
    pub format_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentFormat {
    Pdf,
    Docx,
    Pptx,
    Xlsx,
    Epub,
    Odt,
    Ods,
    Odp,
    Rtf,
    /// RFC 822 / MIME email.
    Eml,
    /// Outlook OLE/MAPI email.
    Msg,
    /// CSV and TSV: one line of text per row.
    Delimited,
    /// Apache Parquet: schema plus the first rows, never the whole file.
    Parquet,
    PlainText,
    Image,
    Unknown,
}

/// Sniffs the format using magic bytes first, falling back to file extension.
pub fn sniff_format(path: &Path) -> DocumentFormat {
    let mut header = [0u8; 16];
    if let Ok(mut f) = File::open(path)
        && let Ok(n) = f.read(&mut header)
        && n >= 4
    {
        // PDF magic: %PDF-
        if header.starts_with(b"%PDF-") {
            return DocumentFormat::Pdf;
        }
        // RTF magic: {\rtf
        if header.starts_with(b"{\\rtf") {
            return DocumentFormat::Rtf;
        }
        // Parquet magic: PAR1
        if header.starts_with(b"PAR1") {
            return DocumentFormat::Parquet;
        }
        // Image magic bytes
        if header.starts_with(b"\x89PNG")
            || header.starts_with(b"\xff\xd8\xff")
            || header.starts_with(b"GIF8")
            || header.starts_with(b"RIFF")
            || header.starts_with(b"BM")
        {
            return DocumentFormat::Image;
        }
        // ZIP magic: PK\x03\x04
        if header.starts_with(b"PK\x03\x04") {
            // Differentiate by extension
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            return match ext.as_str() {
                "docx" => DocumentFormat::Docx,
                "pptx" => DocumentFormat::Pptx,
                "xlsx" => DocumentFormat::Xlsx,
                "epub" => DocumentFormat::Epub,
                "odt" => DocumentFormat::Odt,
                "ods" => DocumentFormat::Ods,
                "odp" => DocumentFormat::Odp,
                _ => DocumentFormat::Docx, // Default zip-office
            };
        }
    }

    // Fallback to extension
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "pdf" => DocumentFormat::Pdf,
        "docx" => DocumentFormat::Docx,
        "pptx" => DocumentFormat::Pptx,
        "xlsx" => DocumentFormat::Xlsx,
        "epub" => DocumentFormat::Epub,
        "odt" => DocumentFormat::Odt,
        "ods" => DocumentFormat::Ods,
        "odp" => DocumentFormat::Odp,
        "rtf" => DocumentFormat::Rtf,
        "eml" => DocumentFormat::Eml,
        "msg" => DocumentFormat::Msg,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "heic" | "heif" => {
            DocumentFormat::Image
        }
        "csv" | "tsv" => DocumentFormat::Delimited,
        "parquet" => DocumentFormat::Parquet,
        "txt" | "md" | "markdown" | "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "json"
        | "toml" | "yaml" | "yml" | "xml" | "html" | "htm" | "css" | "c" | "cpp" | "h" | "java"
        | "sh" | "log" | "sql" => DocumentFormat::PlainText,
        _ => DocumentFormat::Unknown,
    }
}

/// Extracts text content and metadata from documents according to ARCHITECTURE §5.2 content policy:
/// - Max file size: 20MB (files above 20MB are metadata-only).
/// - Extracted content capped at max_bytes (default 1MB).
/// - Panic-isolated with std::panic::catch_unwind.
///
/// Global LRU cache for extraction results, keyed by (path, size, mtime).
static EXTRACTION_CACHE: std::sync::OnceLock<Mutex<ExtractionCache>> = std::sync::OnceLock::new();

struct ExtractionCache {
    capacity: usize,
    map: HashMap<(std::path::PathBuf, u64, u64), DocumentExtract>,
    order: VecDeque<(std::path::PathBuf, u64, u64)>,
}

impl ExtractionCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &(std::path::PathBuf, u64, u64)) -> Option<DocumentExtract> {
        if let Some(val) = self.map.get(key) {
            // Move to back (most recently used)
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
            self.order.push_back(key.clone());
            return Some(val.clone());
        }
        None
    }

    fn insert(&mut self, key: (std::path::PathBuf, u64, u64), val: DocumentExtract) {
        if self.map.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.map.insert(key, val);
    }
}

fn get_extraction_cache() -> &'static Mutex<ExtractionCache> {
    // A small cache is deliberate: each entry can hold up to a megabyte of
    // text, and the crawl no longer depends on cache hits (it extracts once and
    // feeds both the lexical and semantic index). Keeping 16 entries bounds
    // this at a few megabytes instead of tens of megabytes.
    EXTRACTION_CACHE.get_or_init(|| Mutex::new(ExtractionCache::new(16)))
}

/// Cache key for a path: `(path, size, mtime)`.
///
/// Shared by [`extract_document`] and [`cached_document`] so a lookup never
/// disagrees with what was inserted.
fn cache_key(path: &Path) -> Option<(PathBuf, u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((path.to_path_buf(), size, mtime))
}

/// Returns a previously extracted document from the LRU cache, if present.
///
/// This is a **cache-only** lookup: it never reads, parses, OCRs or otherwise
/// extracts anything. Query-time callers use it so a snippet can be produced
/// for a document without paying the PDF/Office/OCR cost. Returns `None` when
/// the path is not cached (or cannot be stat'ed).
pub fn cached_document(path: &Path) -> Option<DocumentExtract> {
    let key = cache_key(path)?;
    let mut cache = get_extraction_cache().lock().ok()?;
    cache.get(&key)
}

/// Inserts an extraction result into the process-wide cache.
///
/// Test-only: production inserts happen in [`extract_document`].
#[cfg(test)]
pub fn insert_cached_document(path: &Path, document: DocumentExtract) {
    let Some(key) = cache_key(path) else {
        return;
    };
    let mut cache = get_extraction_cache().lock().unwrap();
    cache.insert(key, document);
}

/// Upper bound for a single document extraction. Plain documents use a snappy
/// 15 s timeout; OCR candidates (images and PDFs) get 45 s to account for
/// deep neural net inference queues and complex screenshot recognition.
fn extraction_timeout_for(file_size: u64, is_ocr_candidate: bool) -> Duration {
    let base = if is_ocr_candidate { 45 } else { 15 };
    let extra = file_size / (5 * 1024 * 1024) * 2;
    let secs = (base + extra).min(120);
    Duration::from_secs(secs)
}

/// How long a caller waits for a free extraction slot before giving up on the
/// file for this crawl. This is backpressure, not a work timeout: without it a
/// stalled document could permanently consume every slot.
const SLOT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of concurrent extractions (and therefore concurrent OCR processes).
fn extraction_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8)
}

/// Bounded slot pool paired with the extraction thread pool.
///
/// Implemented via a condition variable and atomic counting to eliminate
/// mutex-held receiver serialization: multiple callers can wait simultaneously
/// without starving each other, and freed slots wake waiting callers immediately.
struct SlotPool {
    available: Mutex<usize>,
    cvar: Condvar,
}

impl SlotPool {
    fn new(slots: usize) -> Self {
        Self {
            available: Mutex::new(slots),
            cvar: Condvar::new(),
        }
    }

    fn acquire(&'static self) -> Option<ExtractionSlot> {
        self.acquire_timeout(SLOT_WAIT_TIMEOUT)
    }

    fn acquire_timeout(&'static self, timeout: Duration) -> Option<ExtractionSlot> {
        let mut available = self.available.lock().ok()?;
        let start = Instant::now();
        while *available == 0 {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return None;
            }
            let remaining = timeout - elapsed;
            let (next, wait_res) = self.cvar.wait_timeout(available, remaining).ok()?;
            available = next;
            if wait_res.timed_out() && *available == 0 {
                return None;
            }
        }
        *available -= 1;
        Some(ExtractionSlot { pool: self })
    }
}

struct ExtractionSlot {
    pool: &'static SlotPool,
}

impl Drop for ExtractionSlot {
    fn drop(&mut self) {
        if let Ok(mut count) = self.pool.available.lock() {
            *count += 1;
            self.pool.cvar.notify_one();
        }
    }
}

static SLOT_POOL: std::sync::OnceLock<SlotPool> = std::sync::OnceLock::new();

fn slot_pool() -> &'static SlotPool {
    SLOT_POOL.get_or_init(|| SlotPool::new(extraction_concurrency()))
}

/// Dedicated pool for extraction work.
static EXTRACTION_POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();

fn extraction_pool() -> Option<&'static rayon::ThreadPool> {
    EXTRACTION_POOL
        .get_or_init(|| {
            // Extraction (notably OCR) is the crawl bottleneck. Tesseract runs
            // as a child process, so a few extra workers use the cores without
            // pinning much memory. Capped at 8 to keep the number of concurrent
            // OCR processes (and their RSS) bounded on a desktop.
            match rayon::ThreadPoolBuilder::new()
                .num_threads(extraction_concurrency())
                .thread_name(|index| format!("kensaku-extract-{}", index))
                .build()
            {
                Ok(pool) => Some(pool),
                Err(e) => {
                    eprintln!(
                        "[Kensaku] Could not create extraction thread pool ({}); extraction runs inline without a timeout",
                        e
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Paths that already produced a timeout warning, so a permanently stuck
/// document is reported once instead of on every crawl.
static TIMEOUT_WARNED: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();

fn warn_extraction_timeout(path: &Path, timeout: Duration) {
    let warned = TIMEOUT_WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut warned = warned.lock().unwrap();
    // Keep the bookkeeping bounded on systems with many broken documents.
    if warned.len() >= 4096 {
        warned.clear();
    }
    if warned.insert(path.to_path_buf()) {
        eprintln!(
            "[Kensaku] Text extraction timed out after {}s; leaving pending: {}",
            timeout.as_secs(),
            path.display()
        );
    }
}

/// Reported once per path, and at most a handful of times per process, so a
/// broken plugin does not flood the log during a crawl.
fn warn_external_failure(path: &Path, error: &str) {
    external::record_failure(path, error);
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    static WARNED: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut warned = warned.lock().unwrap();
    if warned.len() >= 4096 {
        warned.clear();
    }
    if warned.insert(path.to_path_buf()) && COUNT.fetch_add(1, Ordering::Relaxed) < 20 {
        eprintln!(
            "[Kensaku] External extraction failed for {}: {}",
            path.display(),
            error
        );
    }
}

/// Reported at most a handful of times so a saturated crawl does not flood the
/// log once per file.
fn warn_slot_saturation(path: &Path) {
    use std::sync::atomic::AtomicUsize;
    static WARNED: AtomicUsize = AtomicUsize::new(0);
    let count = WARNED.fetch_add(1, Ordering::Relaxed);
    if count < 5 {
        eprintln!(
            "[Kensaku] Extraction slots saturated; leaving pending: {}",
            path.display()
        );
    }
}

pub fn extract_document(path: &Path, max_bytes: u64) -> Option<DocumentExtract> {
    extract_document_checked(path, max_bytes).0
}

/// Like [`extract_document`], plus whether the extraction was *incomplete*:
/// it timed out, found every slot busy, or needed an OCR engine that could not
/// run. The indexer keeps such files pending and retries them instead of
/// recording "no text" for good.
pub fn extract_document_checked(path: &Path, max_bytes: u64) -> (Option<DocumentExtract>, bool) {
    let retry = std::sync::Arc::new(AtomicBool::new(false));
    let document = extract_document_tracked(path, max_bytes, &retry);
    (document, retry.load(Ordering::SeqCst))
}

fn extract_document_tracked(
    path: &Path,
    max_bytes: u64,
    retry: &std::sync::Arc<AtomicBool>,
) -> Option<DocumentExtract> {
    let meta = std::fs::metadata(path).ok()?;
    let file_size = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    // Content policy: metadata-only above the configured file-size cap.
    // CSV/TSV/Parquet are sampled from the head, so their cost does not grow
    // with file size and a huge export is still worth indexing.
    let head_sampled = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ["csv", "tsv", "parquet"].iter().any(|s| ext.eq_ignore_ascii_case(s)));
    if file_size > content_max_file_size_bytes() && !head_sampled {
        return None;
    }

    // External extractors are opt-in and keyed by extension/MIME. Resolve
    // them before the OCR gate so a user-supplied plugin can still process an
    // image when the built-in OCR tier is disabled.
    let external = external::find(path);

    // OCR is a user setting: when it is off, images must never produce text.
    // This runs before the cache lookup so a previously cached OCR result is
    // not reused after the setting is turned off. The sniff is only needed on
    // this path; the normal flow below still checks the cache first.
    if external.is_none() && !ocr_enabled() && sniff_format(path) == DocumentFormat::Image {
        return None;
    }

    // Check cache first
    let cache_key = (path.to_path_buf(), file_size, mtime);
    {
        let mut cache = get_extraction_cache().lock().unwrap();
        if let Some(cached) = cache.get(&cache_key) {
            return Some(cached);
        }
    }

    let capped_bytes = (max_bytes as usize).min(content_max_text_bytes());
    let format = sniff_format(path);
    let is_ocr = format == DocumentFormat::Image || format == DocumentFormat::Pdf;
    let timeout = extraction_timeout_for(file_size, is_ocr);

    // Run the extraction on the dedicated pool and time it out. On timeout the
    // file stays pending (or skipped) and is retried on a later crawl; nothing
    // is cached, so a transient stall is not sticky.
    //
    // A slot is acquired first and moved into the task, so timed-out work can
    // never pile up beyond the pool size (see `SlotPool`).
    let result = match (extraction_pool(), slot_pool().acquire()) {
        (Some(pool), Some(slot)) => {
            let (tx, rx) = std::sync::mpsc::channel();
            let worker_path = path.to_path_buf();
            let worker_external = external.clone();
            let worker_retry = std::sync::Arc::clone(retry);
            pool.spawn(move || {
                // Keep the slot alive for the real duration of the work, even
                // if the caller stopped waiting.
                let _slot = slot;
                // Catch panics here: a panic escaping a `rayon::spawn` task
                // reaches rayon's panic handler (which aborts by default), and
                // a malformed document must never take the app down.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    extract_document_inner(
                        &worker_path,
                        capped_bytes,
                        format,
                        file_size,
                        worker_external,
                        &worker_retry,
                    )
                }))
                .unwrap_or_else(|_| {
                    eprintln!(
                        "[Kensaku] Text extraction panicked; leaving pending: {}",
                        worker_path.display()
                    );
                    None
                });
                // Ignore send errors: the caller may have timed out already.
                let _ = tx.send(result);
            });

            match rx.recv_timeout(timeout) {
                Ok(result) => result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    warn_extraction_timeout(path, timeout);
                    retry.store(true, Ordering::SeqCst);
                    None
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    retry.store(true, Ordering::SeqCst);
                    eprintln!(
                        "[Kensaku] Text extraction worker exited without a result; leaving pending: {}",
                        path.display()
                    );
                    None
                }
            }
        }
        // Every slot stayed busy for the whole wait: leave the file pending
        // rather than adding to an unbounded queue.
        (Some(_), None) => {
            warn_slot_saturation(path);
            retry.store(true, Ordering::SeqCst);
            None
        }
        // Pool creation failed (rare); fall back to inline extraction.
        (None, _) => extract_document_inner(path, capped_bytes, format, file_size, external, retry),
    };

    // Cache the result. Very large documents are skipped: the cache exists for
    // query-time snippets and repeated small extractions, not to pin megabytes
    // of text per entry.
    if let Some(ref res) = result
        && res.text.len() <= 256 * 1024
        && !retry.load(Ordering::SeqCst)
    {
        let mut cache = get_extraction_cache().lock().unwrap();
        cache.insert(cache_key, res.clone());
    }

    crate::util::memory::release_memory();
    result
}

fn extract_document_inner(
    path: &Path,
    capped_bytes: usize,
    format: DocumentFormat,
    file_size: u64,
    external: Option<ExternalExtractor>,
    retry: &AtomicBool,
) -> Option<DocumentExtract> {
    // OCR could not run, but an engine *should* be usable (it was a moment
    // ago, or is still loading): retry later. When no engine exists at all
    // the file is done for now; enabling one re-queues every image through
    // the OCR signature (see `IndexerManager::sync_ocr`).
    let ocr_retry = || {
        if crate::ocr::usable_engine() != crate::ocr::NONE {
            retry.store(true, Ordering::SeqCst);
        }
    };

    if let Some(extractor) = external {
        return match extractor.run(path, capped_bytes) {
            Ok(document) => Some(document),
            Err(error) => {
                warn_external_failure(path, &error);
                None
            }
        };
    }

    match format {
        DocumentFormat::Pdf => {
            let res = pdf::extract_pdf(path, capped_bytes)?;
            if res.ocr_unavailable {
                ocr_retry();
            }
            Some(DocumentExtract {
                text: res.text,
                page_count: Some(res.page_count),
                is_encrypted: res.is_encrypted,
                is_scanned: res.is_scanned,
                format_name: "PDF".to_string(),
            })
        }
        DocumentFormat::Docx => {
            let text = office::extract_docx(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Word Document".to_string(),
            })
        }
        DocumentFormat::Pptx => {
            let text = office::extract_pptx(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Presentation".to_string(),
            })
        }
        DocumentFormat::Xlsx => {
            let text = office::extract_xlsx(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Spreadsheet".to_string(),
            })
        }
        DocumentFormat::Epub => {
            let text = office::extract_epub(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "EPUB eBook".to_string(),
            })
        }
        DocumentFormat::Odt | DocumentFormat::Ods | DocumentFormat::Odp => {
            let text = office::extract_odt(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "OpenDocument".to_string(),
            })
        }
        DocumentFormat::Rtf => {
            let text = text::extract_rtf(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Rich Text".to_string(),
            })
        }
        DocumentFormat::Eml => {
            let text = email::extract_eml(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Email".to_string(),
            })
        }
        DocumentFormat::Msg => {
            let text = email::extract_msg(path, capped_bytes)?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Email".to_string(),
            })
        }
        DocumentFormat::Image => {
            // Check the flag here as well: the inline fallback path and any
            // direct caller must not run OCR when it is disabled.
            if !ocr_enabled() {
                return None;
            }
            let text = match image::ocr_image(path) {
                crate::ocr::OcrOutcome::Text(text) => text,
                // An empty document is a real answer ("this photo has no
                // text"): it replaces stale OCR text and lets the indexer
                // mark the image done.
                crate::ocr::OcrOutcome::NoText => String::new(),
                crate::ocr::OcrOutcome::Unavailable => {
                    ocr_retry();
                    return None;
                }
            };
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: true,
                format_name: "Image (OCR)".to_string(),
            })
        }
        DocumentFormat::Parquet => {
            let text = parquet::extract_parquet(path, capped_bytes, csv_head_rows())?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Parquet".to_string(),
            })
        }
        DocumentFormat::Delimited => {
            // Rows are kept whole, so this arm never uses the head/tail split below.
            let text = delimited::extract_delimited(path, capped_bytes, csv_head_rows())?;
            Some(DocumentExtract {
                text,
                page_count: None,
                is_encrypted: false,
                is_scanned: false,
                format_name: "Delimited text".to_string(),
            })
        }
        DocumentFormat::PlainText | DocumentFormat::Unknown => {
            // Large plain text: read a head and a tail within the configured
            // content budget.
            //
            // This used to hard-code 1 MB / 512 KB + 512 KB, ignoring
            // `content_max_text_bytes` entirely, so the "deep content" setting
            // could not reach past a megabyte and the middle of every large
            // file was silently unsearchable.
            let budget = capped_bytes.max(1024);
            if file_size > budget as u64 {
                let half = budget / 2;
                let mut f = File::open(path).ok()?;

                let mut head_bytes = vec![0u8; half];
                let n_head = f.read(&mut head_bytes).unwrap_or(0);
                head_bytes.truncate(n_head);
                // Never end the head mid-character.
                let head_end = match std::str::from_utf8(&head_bytes) {
                    Ok(_) => head_bytes.len(),
                    Err(error) => error.valid_up_to(),
                };

                let mut tail_bytes = vec![0u8; half];
                let n_tail = if f
                    .seek(std::io::SeekFrom::End(-(half as i64)))
                    .is_ok()
                {
                    f.read(&mut tail_bytes).unwrap_or(0)
                } else {
                    0
                };
                tail_bytes.truncate(n_tail);
                // Start the tail on a character boundary (skip continuation
                // bytes left by cutting a multi-byte sequence in half).
                let mut tail_start = 0;
                while tail_start < tail_bytes.len().min(3)
                    && (tail_bytes[tail_start] & 0xC0) == 0x80
                {
                    tail_start += 1;
                }

                let head_str = String::from_utf8_lossy(&head_bytes[..head_end]);
                let tail_str = String::from_utf8_lossy(&tail_bytes[tail_start..]);
                let combined = format!(
                    "{}\n\n... [content omitted] ...\n\n{}",
                    head_str, tail_str
                );

                Some(DocumentExtract {
                    text: combined,
                    page_count: None,
                    is_encrypted: false,
                    is_scanned: false,
                    format_name: "Text".to_string(),
                })
            } else {
                let text = text::extract_text(path, capped_bytes)?;
                Some(DocumentExtract {
                    text,
                    page_count: None,
                    is_encrypted: false,
                    is_scanned: false,
                    format_name: "Text".to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A temp file that removes itself on drop.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            let path = Self::path_for(extension);
            std::fs::write(&path, bytes).expect("write temp file");
            Self { path }
        }

        fn empty(extension: &str) -> Self {
            let path = Self::path_for(extension);
            File::create(&path).expect("create temp file");
            Self { path }
        }

        fn path_for(extension: &str) -> PathBuf {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!(
                "kensaku-extract-{}-{}.{}",
                std::process::id(),
                id,
                extension
            ))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn extract(path: &Path) -> (Option<DocumentExtract>, Duration) {
        let started = Instant::now();
        let result = extract_document(path, 1_000_000);
        (result, started.elapsed())
    }

    #[test]
    fn garbage_pdf_returns_none_without_hanging() {
        let file = TempFile::new("pdf", b"this is definitely not a pdf\n\x00\x01\x02\xff");
        let (result, elapsed) = extract(&file.path);

        assert!(
            result.is_none(),
            "garbage PDF should not extract: {result:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn truncated_pdf_header_returns_sane_result() {
        let file = TempFile::new("pdf", b"%PDF-1.4\n");
        let (result, elapsed) = extract(&file.path);

        // A header-only document may fail to load (None) or load as an empty
        // document; either way it must not panic or report pages with content.
        if let Some(doc) = &result {
            assert_eq!(doc.page_count, Some(0));
            assert!(doc.text.is_empty(), "unexpected text: {:?}", doc.text);
        }
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn docx_zip_without_document_xml_returns_none() {
        let file = TempFile::path_for("docx");
        {
            let handle = File::create(&file).expect("create zip");
            let mut zip = zip::ZipWriter::new(handle);
            zip.start_file("word/styles.xml", zip::write::SimpleFileOptions::default())
                .expect("start file");
            zip.write_all(b"<styles/>").expect("write entry");
            zip.finish().expect("finish zip");
        }
        let _cleanup = TempFile { path: file.clone() };

        let (result, elapsed) = extract(&file);

        assert!(
            result.is_none(),
            "docx without word/document.xml should not extract: {result:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn zero_byte_file_returns_none_without_hanging() {
        let file = TempFile::empty("txt");
        let (result, elapsed) = extract(&file.path);

        assert!(
            result.is_none(),
            "empty file should not extract: {result:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    fn cached_text(text: &str) -> DocumentExtract {
        DocumentExtract {
            text: text.to_string(),
            page_count: None,
            is_encrypted: false,
            is_scanned: false,
            format_name: "Text".to_string(),
        }
    }

    #[test]
    fn cached_document_only_consults_the_cache() {
        let file = TempFile::new("txt", b"on-disk text that must never be read");
        let path = &file.path;

        // Nothing has been extracted for this unique path yet.
        assert!(
            cached_document(path).is_none(),
            "a cold cache must not extract on lookup"
        );

        insert_cached_document(path, cached_text("cached needle text"));
        let hit = cached_document(path).expect("cached document");
        assert_eq!(hit.text, "cached needle text");
    }

    #[test]
    fn image_extraction_is_disabled_when_ocr_is_off() {
        // A real, decodable PNG: the result must be `None` because of the
        // setting, not because the file is invalid. Tesseract may or may not
        // be installed, so this never asserts on extracted text.
        let path = TempFile::path_for("png");
        ::image::RgbImage::from_pixel(20, 20, ::image::Rgb([12, 34, 56]))
            .save_with_format(&path, ::image::ImageFormat::Png)
            .expect("write test png");
        let _cleanup = TempFile { path: path.clone() };

        let previous = ocr_enabled();
        set_ocr_enabled(false);
        let result = extract_document(&path, 1_000_000);
        set_ocr_enabled(previous);

        assert!(
            result.is_none(),
            "an image must not produce content while OCR is disabled: {result:?}"
        );
    }
}
