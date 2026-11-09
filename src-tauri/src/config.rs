use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alias {
    /// Word the user types.
    pub term: String,
    /// Other spellings that should match the same thing.
    pub expansions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearch {
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub sort: Option<String>,
}

/// Opt-in external text extractor. Commands are executed directly (never
/// through a shell), with a cleared environment and a hard timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalExtractor {
    /// Shown in extraction metadata and warnings.
    pub name: String,
    /// Lowercase extensions without the dot, e.g. `["stl", "obj"]`.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Optional MIME selectors, e.g. `["model/*"]`. Matched alongside
    /// `extensions` when either matches. `mime_types` is accepted as an alias.
    #[serde(default, alias = "mime_types")]
    pub mime: Vec<String>,
    /// Absolute path to an executable file. PATH lookup is never used.
    pub command: String,
    /// Individual arguments. `{path}` is replaced with the file path; if no
    /// argument contains it, the path is appended as the last argument.
    #[serde(default)]
    pub args: Vec<String>,
    /// Wall-clock limit; clamped to 100..=7000 ms by the runner.
    #[serde(default = "default_extractor_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum captured stdout; clamped to 1 KiB..=4 MiB.
    #[serde(default = "default_extractor_output_bytes")]
    pub max_output_bytes: u64,
    /// External commands never run unless this is explicitly true.
    #[serde(default)]
    pub enabled: bool,
}

pub fn default_extractor_timeout_ms() -> u64 {
    5_000
}

pub fn default_extractor_output_bytes() -> u64 {
    1_000_000
}

pub fn default_aliases() -> Vec<Alias> {
    [
        ("resume", &["cv", "curriculum vitae"][..]),
        ("invoice", &["receipt", "bill"][..]),
        ("receipt", &["invoice", "bill"][..]),
        ("screenshot", &["screen shot", "capture"][..]),
        ("contract", &["agreement"][..]),
        ("photo", &["image", "picture"][..]),
    ]
    .into_iter()
    .map(|(term, expansions)| Alias {
        term: term.to_string(),
        expansions: expansions.iter().map(|s| s.to_string()).collect(),
    })
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CrawlMode {
    Full,
    #[default]
    Balanced,
    Eco,
}

impl CrawlMode {
    /// True when indexing should run at background priority.
    pub fn is_background(self) -> bool {
        !matches!(self, Self::Full)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Balanced => "balanced",
            Self::Eco => "eco",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KensakuConfig {
    pub watched_dirs: Vec<PathBuf>,
    pub excluded_dirs: Vec<String>,
    pub excluded_extensions: Vec<String>,
    pub shortcut: String,
    pub max_results: usize,
    pub is_setup_complete: bool,
    /// True once the *first* full crawl finished. `is_setup_complete` in the
    /// command layer is the AND of this and `is_setup_complete` (configured).
    #[serde(default)]
    pub first_scan_done: bool,
    /// Skip files and directories whose name starts with `.` (hidden entries).
    /// Enabled by default; users can opt into indexing hidden files.
    #[serde(default = "default_true")]
    pub skip_hidden: bool,
    #[serde(default = "default_true")]
    pub semantic_enabled: bool,
    #[serde(default = "default_semantic_model")]
    pub semantic_model: String,
    /// Keep the static embedding table in f32 for one release. The default is
    /// the compact i8 table; this switch exists for quality comparisons and
    /// rollback without rebuilding.
    #[serde(default)]
    pub semantic_f32_table: bool,
    /// Background indexing policy. `balanced` uses idle priority; `eco` also
    /// paces work on battery. `full` stays at normal priority.
    #[serde(default)]
    pub crawl_mode: CrawlMode,
    /// Pause background crawling without stopping the watcher. Renames and
    /// deletes still apply while paused.
    #[serde(default)]
    pub pause_indexing: bool,
    /// Allow MCP agents to call the opt-in `reindex` write tool. Disabled by
    /// default; read-only tools are always available.
    #[serde(default)]
    pub mcp_allow_reindex: bool,
    #[serde(default = "default_true")]
    pub ocr_enabled: bool,
    /// How the app presents itself: `launcher` (hide on blur/Esc, tray only,
    /// tiny footprint) or `app` (stays open, appears in the dock/taskbar).
    #[serde(default = "default_app_mode")]
    pub app_mode: String,
    /// Closing the window keeps the app running in the tray instead of
    /// quitting, so reopening is instant.
    ///
    /// Startup is the expensive part: the metadata database, the full-text
    /// index and the semantic units all have to be loaded before the first
    /// query can run. Quitting on close threw that away and paid for it again
    /// on every open, which on a hundred thousand files is several seconds.
    /// Staying resident keeps the Rust side warm while the webview is
    /// destroyed on close, so the idle footprint is the index, not the UI.
    ///
    /// Quitting is explicit: the tray menu's Quit item, or turning this off.
    #[serde(default = "default_true")]
    pub close_to_background: bool,
    /// OCR backend: `tesseract` (fast default), `accurate` (optional
    /// PP-OCRv6 pack) or `indra` (optional Indra OCR pack). Indra falls back
    /// to PP-OCR, PP-OCR to Tesseract, when a pack is absent.
    #[serde(default = "default_ocr_engine")]
    pub ocr_engine: String,
    /// Opt-in external extractors keyed by extension or MIME type. Empty by
    /// default: no external command ever runs without explicit configuration.
    #[serde(default)]
    pub extractors: Vec<ExternalExtractor>,
    /// Skip OCR on images larger than this many bytes (0 = no limit). Useful
    /// for speeding up the first scan on screenshot-heavy corpora. Default 0.
    #[serde(default)]
    pub ocr_max_image_bytes: u64,
    /// Tesseract language codes, e.g. `["eng", "hin"]`. Empty means "use
    /// English when installed". Previously Tesseract always ran as English
    /// because `-l` was never passed, whatever the user had installed.
    #[serde(default)]
    pub ocr_languages: Vec<String>,
    /// Beta visual-search model pack. This only gates the Beta settings tab and
    /// model download; it is intentionally not wired into search yet.
    #[serde(default)]
    pub visual_search_beta_enabled: bool,
    /// Content depth for large documents. `quick` (default) keeps the historic
    /// caps; `balanced` and `deep` raise them for book/manual-sized files.
    /// Stored as a string so unknown values fall back to `quick`.
    #[serde(default = "default_content_depth")]
    pub content_depth: String,
    /// Maximum file size (in MB) that gets its text extracted. Files above
    /// this are indexed by name/filters only. Default 20.
    #[serde(default = "default_content_max_file_size_mb")]
    pub content_max_file_size_mb: u64,
    /// Maximum extracted text (in bytes) per document. Default 1 MB.
    #[serde(default = "default_content_max_text_bytes")]
    pub content_max_text_bytes: usize,
    /// Maximum semantic units (chunks) per document. Default 20.
    #[serde(default = "default_content_max_semantic_units")]
    pub content_max_semantic_units: usize,
    /// Agent-surface scope: when non-empty, MCP/HTTP/CLI results and snippet
    /// reads are restricted to these roots (read-only scoping per root).
    #[serde(default)]
    pub agent_scoped_roots: Vec<String>,
    /// Include files matched by `.gitignore` / `.cursorignore` in the index.
    /// Default false: ignore files are honored so agents see the same tree
    /// developers do ("ignore parity").
    #[serde(default)]
    pub include_ignored: bool,
    /// Persisted UI theme ("light" or "dark"). Legacy "paper" / "void"
    /// configs are migrated at load (v5).
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Surface finish: "solid" (default), "frost" or "glass". Independent of
    /// `theme`, so every finish works with light and dark.
    #[serde(default = "default_surface")]
    pub surface: String,
    /// UI language code ("en", "zh", …) or None for auto-detect from the
    /// system locale. Added in v7.
    #[serde(default)]
    pub language: Option<String>,
    /// Personal vocabulary: `term` matches any of `expansions`.</arg_value>
    #[serde(default = "default_aliases")]
    pub aliases: Vec<Alias>,
    /// Saved queries shown on the home screen.
    #[serde(default)]
    pub saved_searches: Vec<SavedSearch>,
    /// Pinned file paths shown on the home screen.
    #[serde(default)]
    pub pins: Vec<String>,
    /// Show generated thumbnails on result cards. On by default; off shows
    /// the type icon only (no thumbnail generation at all).
    #[serde(default = "default_true")]
    pub show_thumbnails: bool,
    /// Result layout: "grid" (cards, default) or "list" (one row per file).
    #[serde(default = "default_results_view")]
    pub results_view: String,
    /// Automatically check for updates on startup and in the background.
    /// Enabled by default; users can opt out in settings.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// Config schema version, used for one-time migrations.
    #[serde(default)]
    pub config_version: u32,
}

fn default_true() -> bool {
    true
}

/// PP-OCR ships with every installer and is verified at runtime; where it
/// cannot run (Intel Macs) the engine selection falls back to Tesseract.
fn default_ocr_engine() -> String {
    "accurate".to_string()
}

fn default_app_mode() -> String {
    "launcher".to_string()
}

/// Pure core of [`KensakuConfig::config_dir`], kept out of the env-dependent
/// wrapper so tests can exercise every precedence branch without mutating
/// process env vars (which parallel test threads share).
///
/// - `env`: `KENSAKU_DATA_DIR` when set, `None` otherwise.
/// - `exe_path`: `current_exe()` when resolvable, `None` otherwise.
/// - `platform_default`: `dirs::config_dir()`, allowed to be `None` on
///   exotic platforms, in which case a relative fallback is used.
fn resolve_config_dir(
    env: Option<&std::ffi::OsStr>,
    exe_path: Option<&std::path::Path>,
    platform_default: Option<PathBuf>,
) -> PathBuf {
    if let Some(dir) = env
        && !dir.is_empty()
        && !dir
            .to_str()
            .is_some_and(|text| text.trim().is_empty())
    {
        return PathBuf::from(dir);
    }
    if let Some(exe_dir) = exe_path.and_then(std::path::Path::parent)
        && (exe_dir.join("portable").is_file() || exe_dir.join("kensaku.portable").is_file())
    {
        return exe_dir.join("data");
    }
    if let Some(config_dir) = platform_default {
        config_dir.join("kensaku")
    } else {
        PathBuf::from(".kensaku")
    }
}

fn default_semantic_model() -> String {
    "static".to_string()
}

fn default_results_view() -> String {
    "grid".to_string()
}

fn default_content_depth() -> String {
    "balanced".to_string()
}

/// Books and manuals are often 20-100 MB because of their images while their
/// text is small; below this cap they get full-text search, above it they
/// are still findable by name.
fn default_content_max_file_size_mb() -> u64 {
    100
}

/// About 2,000 pages of text. Extraction of ~6 MB measured 3.8 s, and the
/// extraction timeout scales with file size.
fn default_content_max_text_bytes() -> usize {
    4_000_000
}

/// 48 overlapping chunks covers roughly the first 9,000 words. Only large
/// documents use more than one; each chunk is 256 bytes of vector.
fn default_content_max_semantic_units() -> usize {
    48
}

fn default_theme() -> String {
    // The config default and the inline pre-paint script in index.html must
    // agree on this: if both storage and matchMedia fail, the first frame
    // paints dark. Fresh installs otherwise follow the OS (see useTheme.ts
    // and #72); the frontend owns first-run resolution.
    "dark".to_string()
}

fn default_surface() -> String {
    "solid".to_string()
}

/// The canonical surface finish for a name, or None if it is not one.
pub fn parse_surface(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "solid" => Some("solid"),
        "frost" => Some("frost"),
        "glass" => Some("glass"),
        _ => None,
    }
}

/// Canonicalizes a theme name to "light" or "dark".
///
/// Accepts the current names and the pre-1.1 names ("paper" / "void").
/// Anything unrecognized falls back to the default, which keeps a
/// hand-edited config from wedging the app.
pub fn normalize_theme(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "light" | "paper" => "light".to_string(),
        "dark" | "void" => "dark".to_string(),
        _ => default_theme(),
    }
}

/// Canonicalizes a language code to a supported value, or None for auto-detect.
pub fn normalize_language(raw: &str) -> Option<String> {
    let v = raw.trim().to_ascii_lowercase();
    match v.as_str() {
        "en" | "zh" | "ja" | "es" | "fr" | "de" | "ko" | "pt" | "ru" | "it" => Some(v),
        _ => None,
    }
}

/// Version written by this build; see `parse_and_migrate`.
pub const CURRENT_CONFIG_VERSION: u32 = 7;

/// Extensions skipped entirely (not even indexed by name).
const DEFAULT_EXCLUDED_EXTENSIONS: &[&str] = &[
    // Binaries and build artifacts
    "exe", "dll", "so", "dylib", "bin", "o", "a", "lib", "class", "pyc", "pyo",
    // Secrets / credentials (`key` is also Apple Keynote; private keys win)
    "pem", "key", "pfx", "p12", "crt", "cer",
    // Databases
    "db", "sqlite", "sqlite3",
    // Lock files
    "lock", "lockb",
    // Minified / generated
    "min.js", "min.css", "map",
];

/// Extensions excluded by default up to config v5 that are now findable by
/// name (v6 removes them from existing configs).
const NAME_SEARCHABLE_SINCE_V6: &[&str] = &[
    "psd", "ai", "sketch", "fig", "mp3", "mp4", "wav", "avi", "mov", "mkv", "flac", "ogg",
    "webm", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "tgz", "ttf", "otf", "woff",
    "woff2", "eot",
];

impl Default for KensakuConfig {
    fn default() -> Self {
        let mut watched = Vec::new();
        if let Some(home) = dirs::home_dir() {
            watched.push(home);
        }

        let default_excluded_dirs: Vec<String> = vec![
            // Version control
            ".git",
            ".svn",
            ".hg",
            ".bzr",
            // Package managers / dependencies
            "node_modules",
            "vendor",
            "Pods",
            ".bundle",
            // Build outputs
            "target",
            "build",
            "dist",
            "out",
            ".output",
            ".next",
            ".nuxt",
            ".svelte-kit",
            ".turbo",
            ".vercel",
            ".netlify",
            // Caches
            ".cache",
            ".npm",
            ".yarn",
            ".pnpm",
            ".cargo",
            ".rustup",
            ".gradle",
            ".m2",
            "__pycache__",
            ".pytest_cache",
            ".mypy_cache",
            ".tox",
            ".nox",
            // Virtual environments
            "venv",
            ".venv",
            "env",
            ".env",
            // IDE / editor
            ".vscode",
            ".idea",
            ".vs",
            ".eclipse",
            // OS junk
            ".DS_Store",
            "Thumbs.db",
            "desktop.ini",
            // Misc
            ".Trash",
            "lost+found",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        // Documents (PDF/Office/EPUB) and images are indexed via text extraction
        // and OCR, so they are deliberately NOT excluded by default.
        //
        // Media, archives, fonts and design files are not excluded either:
        // their contents are never read (they are not indexable types), but a
        // search for "vacation.mp4" or "invoice.zip" must still find them by
        // name. Only files nobody searches for, or must not be indexed, stay
        // out entirely.
        let default_excluded_extensions: Vec<String> = DEFAULT_EXCLUDED_EXTENSIONS
            .iter()
            .map(|ext| ext.to_string())
            .collect();

        Self {
            watched_dirs: watched,
            excluded_dirs: default_excluded_dirs,
            excluded_extensions: default_excluded_extensions,
            shortcut: "Alt+Space".to_string(),
            max_results: 50,
            is_setup_complete: false,
            first_scan_done: false,
            skip_hidden: true,
            semantic_enabled: true,
            semantic_model: "static".to_string(),
            semantic_f32_table: false,
            crawl_mode: CrawlMode::default(),
            pause_indexing: false,
            mcp_allow_reindex: false,
            visual_search_beta_enabled: false,
            content_depth: default_content_depth(),
            content_max_file_size_mb: default_content_max_file_size_mb(),
            content_max_text_bytes: default_content_max_text_bytes(),
            content_max_semantic_units: default_content_max_semantic_units(),
            agent_scoped_roots: Vec::new(),
            include_ignored: false,
            extractors: Vec::new(),
            ocr_enabled: true,
            ocr_max_image_bytes: 0,
            ocr_languages: Vec::new(),
            ocr_engine: default_ocr_engine(),
            app_mode: default_app_mode(),
            close_to_background: true,
            theme: default_theme(),
            surface: default_surface(),
            language: None,
            aliases: default_aliases(),
            saved_searches: Vec::new(),
            pins: Vec::new(),
            show_thumbnails: true,
            results_view: default_results_view(),
            auto_update: true,
            config_version: CURRENT_CONFIG_VERSION,
        }
    }
}

impl KensakuConfig {
    /// Resolves Kensaku's data/config directory.
    ///
    /// Precedence:
    /// 1. `KENSAKU_DATA_DIR` env var (explicit override; used by tests and
    ///    Nix/Flatpak/packagers).
    /// 2. Portable mode: if a `portable` or `kensaku.portable` marker file
    ///    sits next to the executable, everything lives in `<exe_dir>/data`.
    /// 3. The platform default (`~/.config/kensaku`, `%APPDATA%\\kensaku`, …).
    pub fn config_dir() -> PathBuf {
        let env_dir = std::env::var_os("KENSAKU_DATA_DIR");
        let exe_path = std::env::current_exe().ok();
        resolve_config_dir(env_dir.as_deref(), exe_path.as_deref(), dirs::config_dir())
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    pub fn index_dir() -> PathBuf {
        Self::config_dir().join("index")
    }

    /// Parses `content` and applies one-time migrations.
    ///
    /// Returns the config plus whether anything changed, so callers that loaded
    /// from disk can persist the migrated form while [`Self::load_from_str`]
    /// stays side-effect free.
    fn parse_and_migrate(content: &str) -> Result<(Self, bool), serde_json::Error> {
        let mut cfg: Self = serde_json::from_str(content)?;
        let mut migrated = false;

        if cfg.config_version < 1 {
            // v1: document/image formats became indexable (text extraction + OCR).
            // Strip the legacy defaults so existing installs start indexing them.
            const LEGACY_EXCLUDED: &[&str] = &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "png", "jpg", "jpeg", "gif",
                "bmp", "ico", "webp", "tiff", "tif",
            ];
            cfg.excluded_extensions.retain(|ext| {
                !LEGACY_EXCLUDED
                    .iter()
                    .any(|legacy| legacy.eq_ignore_ascii_case(ext))
            });
            cfg.config_version = 1;
            migrated = true;
        }

        if cfg.config_version < 2 {
            // v2: early UI configs stored exclusions with a leading dot
            // (".jpg"), so the v1 bare-name strip missed them and documents /
            // images were never indexed. Normalize before comparing.
            const NOW_INDEXABLE: &[&str] = &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "epub", "odt", "ods", "odp",
                "rtf", "png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif",
            ];
            cfg.excluded_extensions.retain(|ext| {
                let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
                !NOW_INDEXABLE.contains(&normalized.as_str())
            });
            cfg.config_version = 2;
            migrated = true;
        }

        if cfg.config_version < 3 {
            // v3: first-scan completion gate. Existing installs that already
            // completed setup are treated as having finished their first scan.
            if cfg.is_setup_complete {
                cfg.first_scan_done = true;
            }
            cfg.config_version = 3;
            migrated = true;
        }

        if cfg.config_version < 4 {
            // v4: default-deny common secret/credential file extensions.
            const SENSITIVE_EXTS: &[&str] = &["pem", "key", "pfx", "p12", "crt", "cer"];
            for ext in SENSITIVE_EXTS {
                if !cfg
                    .excluded_extensions
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(ext))
                {
                    cfg.excluded_extensions.push((*ext).to_string());
                }
            }
            cfg.config_version = 4;
            migrated = true;
        }

        if cfg.config_version < 5 {
            // v5: the UI renamed its two themes from "paper" / "void" to
            // "light" / "dark". Carry the user's pick over instead of
            // silently dropping them back to the default.
            cfg.theme = normalize_theme(&cfg.theme);
            cfg.config_version = 5;
            migrated = true;
        }

        if cfg.config_version < 6 {
            // v6: better defaults (1.0.10). Only values still on an old
            // default move; anything the user changed is left alone.
            //
            // - OCR: PP-OCR now ships with every installer and is verified at
            //   runtime, so the old default engine moves to it (Tesseract
            //   remains the automatic fallback where PP-OCR cannot run).
            if cfg.ocr_engine.eq_ignore_ascii_case("tesseract") {
                // v6 introduced PP-OCR as the default; keep that migration
                // anchored to PP-OCR rather than following future default changes.
                cfg.ocr_engine = "accurate".to_string();
            }
            // - Media, archives, fonts and design files become findable by
            //   name (their contents are still never read).
            cfg.excluded_extensions.retain(|ext| {
                let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
                !NAME_SEARCHABLE_SINCE_V6.contains(&normalized.as_str())
            });
            // - `.Trash-*` never matched: exclusions are exact names.
            cfg.excluded_dirs.retain(|dir| dir != ".Trash-*");
            // - Content depth: raise the caps that are still at the old
            //   defaults so books and manuals become searchable.
            if cfg.content_max_file_size_mb == 20 {
                cfg.content_max_file_size_mb = default_content_max_file_size_mb();
            }
            if cfg.content_max_text_bytes == 1_000_000 {
                cfg.content_max_text_bytes = default_content_max_text_bytes();
            }
            if cfg.content_max_semantic_units == 20 {
                cfg.content_max_semantic_units = default_content_max_semantic_units();
            }
            if cfg.content_depth == "quick" {
                cfg.content_depth = default_content_depth();
            }
            cfg.config_version = 6;
            migrated = true;
        }

        if cfg.config_version < 7 {
            // v7: UI language support. Normalize any persisted value; unknown
            // codes fall back to auto-detect (None).
            cfg.language = cfg.language.as_deref().and_then(normalize_language);
            cfg.config_version = 7;
            migrated = true;
        }

        Ok((cfg, migrated))
    }

    /// Disk-free equivalent of [`Self::load`], used by tests to exercise the
    /// migration path without touching the user's config directory.
    pub fn load_from_str(content: &str) -> Self {
        match Self::parse_and_migrate(content) {
            Ok((cfg, _migrated)) => cfg,
            Err(_) => Self::default(),
        }
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if !path.exists() {
            return Self::default();
        }

        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => {
                eprintln!(
                    "[Kensaku] Could not read config {}: {}; using defaults",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };

        match Self::parse_and_migrate(&content) {
            Ok((cfg, migrated)) => {
                if migrated {
                    let _ = cfg.save();
                }
                cfg
            }
            Err(e) => {
                // Preserve the unreadable file instead of silently discarding
                // the user's settings: the next `save()` will overwrite it.
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = Self::config_dir().join(format!("config.json.corrupt-{}", stamp));
                match fs::copy(&path, &backup) {
                    Ok(_) => eprintln!(
                        "[Kensaku] Config is corrupt ({}); backup saved to {}; using defaults",
                        e,
                        backup.display()
                    ),
                    Err(copy_err) => eprintln!(
                        "[Kensaku] Config is corrupt ({}); failed to back up {}: {}; using defaults",
                        e,
                        path.display(),
                        copy_err
                    ),
                }
                Self::default()
            }
        }
    }

    /// Atomically persists the config: write a temp file, fsync it, rename it
    /// over the real config, then (best effort on unix) fsync the directory so
    /// the rename itself is durable.
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::config_dir())
    }

    /// Writes `<dir>/config.json` atomically. Extracted from [`Self::save`] so
    /// tests can target a temporary directory instead of the real one.
    pub fn save_to(&self, dir: &Path) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = dir.join("config.json");
        let tmp_path = dir.join("config.json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;

        {
            let mut file = fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
            file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }

        fs::rename(&tmp_path, &path).map_err(|e| e.to_string())?;

        #[cfg(unix)]
        {
            // `File::sync_all` on the directory flushes the rename entry. Some
            // filesystems reject fsync on directories, hence best effort.
            if let Ok(dir_file) = fs::File::open(dir) {
                let _ = dir_file.sync_all();
            }
        }

        Ok(())
    }

    pub fn is_setup_done() -> bool {
        let cfg = Self::load();
        cfg.is_setup_complete && cfg.first_scan_done
    }

    /// Alias table for the intent compiler: (term, expansions).
    pub fn alias_pairs(&self) -> Vec<(String, Vec<String>)> {
        self.aliases
            .iter()
            .filter(|a| !a.term.trim().is_empty() && !a.expansions.is_empty())
            .map(|a| (a.term.trim().to_lowercase(), a.expansions.clone()))
            .collect()
    }

    pub fn is_dir_excluded(&self, dir_name: &str) -> bool {
        self.excluded_dirs
            .iter()
            .any(|ex| ex.eq_ignore_ascii_case(dir_name))
    }

    pub fn is_extension_excluded(&self, path: &Path) -> bool {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext = ext.trim_start_matches('.').to_ascii_lowercase();
            return self
                .excluded_extensions
                .iter()
                .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-config-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn auto_update_default_and_roundtrip() {
        let mut cfg = KensakuConfig::default();
        assert!(cfg.auto_update);
        cfg.auto_update = false;
        let json = serde_json::to_string(&cfg).unwrap();
        let loaded = KensakuConfig::load_from_str(&json);
        assert!(!loaded.auto_update);

        let json_without_flag = json.replace("\"auto_update\":false,", "");
        let loaded_defaulted = KensakuConfig::load_from_str(&json_without_flag);
        assert!(loaded_defaulted.auto_update);
    }

    #[test]
    fn closing_keeps_the_app_running_by_default() {
        // The default matters more than the setting: a corpus this large takes
        // seconds to load, and quitting on close paid that cost on every open.
        // An existing config file written before this field must also get the
        // default, or the fix silently does not apply to anyone upgrading.
        let cfg = KensakuConfig::default();
        assert!(
            cfg.close_to_background,
            "closing the window must keep the app running by default"
        );

        let json = serde_json::to_string(&cfg).unwrap();
        let without = json.replace("\"close_to_background\":true,", "");
        assert!(
            !without.contains("close_to_background"),
            "test setup failed to remove the field"
        );
        let loaded = KensakuConfig::load_from_str(&without);
        assert!(
            loaded.close_to_background,
            "a config without the field must default to running in the background"
        );
    }

    #[test]
    fn default_config_is_current_version_and_dark() {
        let cfg = KensakuConfig::default();
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert!(cfg.skip_hidden);
        assert!(cfg.auto_update);
        assert!(cfg.ocr_enabled);
        assert_eq!(cfg.theme, "dark");
    }

    fn write_marker(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn config_dir_env_var_wins_over_everything() {
        let marker_dir = temp_dir("marker");
        std::fs::create_dir_all(&marker_dir).unwrap();
        write_marker(&marker_dir, "portable");
        let default = PathBuf::from("/platform/default");

        let resolved = resolve_config_dir(
            Some(std::ffi::OsStr::new("/override/data")),
            Some(&marker_dir),
            Some(default),
        );
        assert_eq!(resolved, PathBuf::from("/override/data"));
    }

    #[test]
    fn config_dir_empty_env_var_is_ignored() {
        let resolved = resolve_config_dir(
            Some(std::ffi::OsStr::new("  ")),
            None,
            Some(std::env::temp_dir()),
        );
        assert_eq!(
            resolved,
            std::env::temp_dir().join("kensaku"),
            "an empty/whitespace KENSAKU_DATA_DIR must not become an empty relative path"
        );
    }

    #[test]
    fn portable_marker_file_puts_data_next_to_the_exe() {
        let exe_dir = temp_dir("portable");
        write_marker(&exe_dir, "portable");
        let resolved = resolve_config_dir(None, Some(&exe_dir.join("kensaku.exe")), None);
        assert_eq!(resolved, exe_dir.join("data"));
    }

    #[test]
    fn kensaku_portable_marker_file_also_deployed() {
        let exe_dir = temp_dir("portable2");
        write_marker(&exe_dir, "kensaku.portable");
        let resolved = resolve_config_dir(None, Some(&exe_dir.join("kensaku.exe")), None);
        assert_eq!(resolved, exe_dir.join("data"));
    }

    #[test]
    fn portable_marker_directory_named_portable_is_not_a_marker() {
        // `portable` must be a FILE; a directory of that name (someone's
        // folder layout) must not silently redirect data storage.
        let exe_dir = temp_dir("marker-dir");
        std::fs::create_dir_all(exe_dir.join("portable")).unwrap();
        let resolved = resolve_config_dir(
            None,
            Some(&exe_dir.join("kensaku.exe")),
            Some(std::env::temp_dir()),
        );
        assert_eq!(resolved, std::env::temp_dir().join("kensaku"));
    }

    #[test]
    fn no_env_no_marker_uses_platform_default() {
        let resolved = resolve_config_dir(
            None,
            Some(&std::env::temp_dir().join("kensaku.exe")),
            Some(std::env::temp_dir()),
        );
        assert!(resolved.ends_with("kensaku"));
    }

    #[test]
    fn missing_platform_default_falls_back_relative() {
        let resolved = resolve_config_dir(None, None, None);
        assert_eq!(resolved, PathBuf::from(".kensaku"));
    }

    #[test]
    fn semantic_f32_table_flag_defaults_off_and_round_trips() {
        let cfg = KensakuConfig::default();
        assert!(!cfg.semantic_f32_table);
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed = KensakuConfig::load_from_str(&json);
        assert!(!parsed.semantic_f32_table);

        let with_flag = json.replace(
            "\"semantic_f32_table\":false",
            "\"semantic_f32_table\":true",
        );
        assert!(KensakuConfig::load_from_str(&with_flag).semantic_f32_table);
    }

    #[test]
    fn v6_moves_old_defaults_and_keeps_user_choices() {
        let old_defaults = r#"{
            "watched_dirs": [], "excluded_dirs": [".git", ".Trash-*"],
            "excluded_extensions": ["mp4", ".zip", "ttf", "exe", "pem", "key"],
            "shortcut": "Alt+Space", "max_results": 50, "is_setup_complete": true,
            "ocr_engine": "tesseract", "content_max_file_size_mb": 20,
            "content_max_text_bytes": 1000000, "content_max_semantic_units": 20,
            "content_depth": "quick", "config_version": 5
        }"#;
        let cfg = KensakuConfig::load_from_str(old_defaults);
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.ocr_engine, "accurate");
        assert_eq!(cfg.excluded_extensions, vec!["exe", "pem", "key"]);
        assert_eq!(cfg.excluded_dirs, vec![".git"]);
        assert_eq!(cfg.content_max_file_size_mb, 100);
        assert_eq!(cfg.content_max_text_bytes, 4_000_000);
        assert_eq!(cfg.content_max_semantic_units, 48);
        assert_eq!(cfg.content_depth, "balanced");

        // Values the user changed stay exactly as they were.
        let customized = r#"{
            "watched_dirs": [], "excluded_dirs": [], "excluded_extensions": ["log"],
            "shortcut": "Alt+Space", "max_results": 50, "is_setup_complete": true,
            "ocr_engine": "accurate", "content_max_file_size_mb": 5,
            "content_max_text_bytes": 200000, "content_max_semantic_units": 8,
            "config_version": 5
        }"#;
        let cfg = KensakuConfig::load_from_str(customized);
        assert_eq!(cfg.excluded_extensions, vec!["log"]);
        assert_eq!(cfg.content_max_file_size_mb, 5);
        assert_eq!(cfg.content_max_text_bytes, 200_000);
        assert_eq!(cfg.content_max_semantic_units, 8);
    }

    #[test]
    fn fresh_defaults_are_the_1_0_10_defaults() {
        let cfg = KensakuConfig::default();
        assert_eq!(cfg.ocr_engine, "accurate");
        assert!(!cfg.excluded_extensions.iter().any(|e| e == "mp4" || e == "zip"));
        assert!(cfg.excluded_extensions.iter().any(|e| e == "exe"));
        assert!(!cfg.excluded_dirs.iter().any(|d| d.contains('*')));
        assert_eq!(cfg.content_max_file_size_mb, 100);
        // A fresh config round-trips without migrating anything.
        let json = serde_json::to_string(&cfg).unwrap();
        let (_, migrated) = KensakuConfig::parse_and_migrate(&json).unwrap();
        assert!(!migrated);
    }

    #[test]
    fn crawl_mode_and_pause_parse_from_config() {
        let cfg = KensakuConfig::default();
        // One default whether the config is fresh or the key is missing.
        assert_eq!(cfg.crawl_mode, CrawlMode::Balanced);
        assert_eq!(cfg.crawl_mode, CrawlMode::default());
        assert!(!cfg.pause_indexing);

        let raw = r#"{
            "watched_dirs": [],
            "excluded_dirs": [],
            "excluded_extensions": [],
            "shortcut": "Alt+Space",
            "max_results": 50,
            "is_setup_complete": false,
            "crawl_mode": "eco",
            "pause_indexing": true
        }"#;
        let parsed = KensakuConfig::load_from_str(raw);
        assert_eq!(parsed.crawl_mode, CrawlMode::Eco);
        assert!(parsed.pause_indexing);
    }

    #[test]
    fn legacy_config_is_migrated() {
        let legacy = r#"{
            "watched_dirs": [],
            "excluded_dirs": [],
            "excluded_extensions": ["pdf", "PNG", "doc", "rs", "png", "zip"],
            "shortcut": "Alt+Space",
            "max_results": 50,
            "is_setup_complete": false
        }"#;

        let cfg = KensakuConfig::load_from_str(legacy);

        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        // Legacy document/image extensions are stripped (case-insensitively)...
        assert!(
            !cfg.excluded_extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case("pdf"))
        );
        assert!(
            !cfg.excluded_extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case("png"))
        );
        assert!(
            !cfg.excluded_extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case("doc"))
        );
        // ...while user-defined exclusions survive.
        assert!(cfg.excluded_extensions.iter().any(|e| e == "rs"));
        // v6 makes archives findable by name.
        assert!(!cfg.excluded_extensions.iter().any(|e| e == "zip"));
        // Missing defaults are filled in by serde.
        assert!(cfg.skip_hidden);
        assert!(cfg.ocr_enabled);
        assert_eq!(cfg.theme, "dark");
    }

    #[test]
    fn already_migrated_config_is_unchanged() {
        let current = r#"{
            "watched_dirs": [],
            "excluded_dirs": [],
            "excluded_extensions": ["rs", "node_modules"],
            "shortcut": "Alt+Space",
            "max_results": 50,
            "is_setup_complete": false,
            "config_version": 2
        }"#;

        let cfg = KensakuConfig::load_from_str(current);
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(
            cfg.excluded_extensions,
            vec![
                "rs".to_string(),
                "node_modules".to_string(),
                "pem".to_string(),
                "key".to_string(),
                "pfx".to_string(),
                "p12".to_string(),
                "crt".to_string(),
                "cer".to_string(),
            ]
        );
    }

    #[test]
    fn v5_migration_renames_legacy_theme_names() {
        for (stored, expected) in [("paper", "light"), ("void", "dark")] {
            let legacy = format!(
                r#"{{
                "watched_dirs": [],
                "excluded_dirs": [],
                "excluded_extensions": [],
                "shortcut": "Alt+Space",
                "max_results": 50,
                "is_setup_complete": true,
                "theme": "{}",
                "config_version": 4
            }}"#,
                stored
            );

            let cfg = KensakuConfig::load_from_str(&legacy);
            assert_eq!(cfg.theme, expected, "theme {} should become {}", stored, expected);
            assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        }
    }

    #[test]
    fn surface_names_parse_and_default_to_solid() {
        assert_eq!(parse_surface(" Frost "), Some("frost"));
        assert_eq!(parse_surface("GLASS"), Some("glass"));
        assert_eq!(parse_surface("solid"), Some("solid"));
        assert_eq!(parse_surface("liquid"), None);
        assert_eq!(KensakuConfig::default().surface, "solid");
    }

    #[test]
    fn v5_migration_keeps_current_theme_names_and_repairs_junk() {
        assert_eq!(normalize_theme("light"), "light");
        assert_eq!(normalize_theme("DARK"), "dark");
        assert_eq!(normalize_theme("  Void "), "dark");
        assert_eq!(normalize_theme("solarized"), default_theme());
    }

    #[test]
    fn v7_migration_normalizes_language() {
        assert_eq!(normalize_language("ZH"), Some("zh".to_string()));
        assert_eq!(normalize_language(" en "), Some("en".to_string()));
        assert_eq!(normalize_language("klingon"), None);

        let legacy = r#"{
            "watched_dirs": [],
            "excluded_dirs": [],
            "excluded_extensions": [],
            "shortcut": "Alt+Space",
            "max_results": 50,
            "is_setup_complete": true,
            "language": "ZH",
            "config_version": 6
        }"#;
        let cfg = KensakuConfig::load_from_str(legacy);
        assert_eq!(cfg.language.as_deref(), Some("zh"));
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);

        let junk = legacy.replace("\"ZH\"", "\"klingon\"");
        assert_eq!(KensakuConfig::load_from_str(&junk).language, None);
    }

    #[test]
    fn v2_migration_strips_dotted_document_and_image_extensions() {
        let legacy = r#"{
            "watched_dirs": [],
            "excluded_dirs": [],
            "excluded_extensions": [".jpg", ".PNG", ".pdf", ".o", "zip"],
            "shortcut": "Alt+Space",
            "max_results": 50,
            "is_setup_complete": false,
            "config_version": 1
        }"#;

        let cfg = KensakuConfig::load_from_str(legacy);
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(
            cfg.excluded_extensions,
            vec![
                ".o".to_string(),
                "pem".to_string(),
                "key".to_string(),
                "pfx".to_string(),
                "p12".to_string(),
                "crt".to_string(),
                "cer".to_string(),
            ]
        );
    }

    #[test]
    fn external_extractors_default_to_off_with_safe_limits() {
        let default = KensakuConfig::default();
        assert!(default.extractors.is_empty(), "no plugins ship by default");

        let extractor: ExternalExtractor =
            serde_json::from_str(r#"{"name":"sample","extensions":[".STL"],"command":"/bin/cat"}"#)
                .expect("minimal extractor should parse");
        assert!(!extractor.enabled, "plugins require explicit opt-in");
        assert_eq!(extractor.timeout_ms, 5_000);
        assert_eq!(extractor.max_output_bytes, 1_000_000);
        assert!(extractor.mime.is_empty());
        assert!(extractor.args.is_empty());

        let enabled: ExternalExtractor = serde_json::from_str(
            r#"{"name":"sample","extensions":["stl"],"mime":["model/*"],"command":"/bin/cat","enabled":true}"#,
        )
        .expect("explicitly enabled extractor should parse");
        assert!(enabled.enabled);
        assert_eq!(enabled.mime, vec!["model/*".to_string()]);
    }

    #[test]
    fn corrupt_config_falls_back_to_defaults() {
        let cfg = KensakuConfig::load_from_str("{ not json");
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.theme, "dark");
    }

    #[test]
    fn save_to_writes_atomically() {
        let dir = temp_dir("save");

        let cfg = KensakuConfig::default();
        cfg.save_to(&dir).expect("save should succeed");

        let path = dir.join("config.json");
        assert!(path.is_file(), "config.json should exist");
        assert!(
            !dir.join("config.json.tmp").exists(),
            "temp file should be renamed away"
        );

        let saved: KensakuConfig =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(saved.theme, "dark");

        // A second save replaces the existing file atomically.
        let updated = KensakuConfig {
            max_results: 7,
            ..cfg
        };
        updated.save_to(&dir).expect("second save should succeed");
        let saved: KensakuConfig =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.max_results, 7);
        assert!(!dir.join("config.json.tmp").exists());

        fs::remove_dir_all(&dir).ok();
    }
}
