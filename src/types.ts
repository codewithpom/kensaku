/* ---- get_snippets DTO. Snippets are fetched in a second phase after ---- */
/* ---- results land; `null` means "no snippet for this path".          ---- */

export interface SnippetDto {
  path: string;
  snippet: string | null;
}

export interface TransferReport {
  done: string[];
  skipped: number;
  failed: { path: string; error: string }[];
}

export interface SearchResult {
  path: string;
  filename: string;
  snippet: string | null;
  score: number;
  file_size: number;
  modified: number;
  is_dir: boolean;
  match_type: "FileName" | "Content" | "ContentFolded" | "Semantic" | "Visual" | "Both";
  best_range?: [number, number] | null;
  group_count?: number;
  group_paths?: string[];
}

export interface SimilarImageResult {
  path: string;
  filename: string;
  similarity: number;
  file_size: number;
  modified: number;
}

export interface HomeItem {
  path: string;
  filename: string;
  size: number;
  modified: number;
  is_dir: boolean;
  /** Last open through Kensaku (0 = never opened via the launcher). */
  last_open: number;
  /** Total opens through Kensaku. */
  opens: number;
}

export interface ExternalExtractor {
  name: string;
  extensions?: string[];
  mime?: string[];
  command: string;
  args?: string[];
  timeout_ms?: number;
  max_output_bytes?: number;
  enabled?: boolean;
}

export interface SavedSearch {
  name: string;
  query: string;
  sort?: string | null;
}

export interface HomeData {
  recent: HomeItem[];
  pins: HomeItem[];
  saved_searches: SavedSearch[];
  recent_queries?: string[];
}

export interface ActiveFilter {
  raw: string;
  filter_type: string;
  value: string;
  negated: boolean;
}

export interface InterpretationChip {
  kind: string;
  label: string;
  raw: string;
  source: string;
}

export interface Facets {
  all: number;
  types: Record<string, number>;
  languages: Record<string, number>;
  places: Record<string, number>;
  time: Record<string, number>;
  size: Record<string, number>;
  recent: number;
}

export interface Alias {
  term: string;
  expansions: string[];
}

export interface SearchResponse {
  results: SearchResult[];
  interpretation?: InterpretationChip[];
  facets?: Facets;
  seq: number;
  active_filters: ActiveFilter[];
  hints: string[];
  search_time_ms: number;
  /** Matches before the display limit (collected count for exhaustive mode). */
  total?: number;
  /** "ranked" or "exhaustive". */
  mode?: string;
}

export interface FilePreview {
  path: string;
  filename: string;
  file_size: number;
  modified: number;
  is_dir: boolean;
  preview_type: "text" | "image" | "pdf" | "doc" | "presentation" | "sheet" | "dir" | "binary";
  content?: string | null;
  total_lines?: number | null;
  child_count?: number | null;
  /** Pixel dimensions, image previews only — best-effort from the backend. */
  width?: number | null;
  height?: number | null;
}

export interface DuplicateGroup {
  kind: "content" | "burst" | "image";
  hash: string;
  paths: string[];
}

export interface CommandOutput {
  success: boolean;
  message: string;
}

export interface IndexingStatus {
  is_indexing: boolean;
  files_indexed: number;
  content_indexed: number;
  current_status: string;
  is_embedding?: boolean;
  units_indexed?: number;
  semantic_ready?: boolean;
  phase?:
    | "idle"
    | "rebuilding"
    | "walking"
    | "extracting"
    | "embedding"
    | "committing"
    | "paused"
    | "error";
  total_files?: number;
  extract_done?: number;
  extract_total?: number;
  paused?: boolean;
}

export interface KensakuConfig {
  watched_dirs: string[];
  excluded_dirs: string[];
  excluded_extensions: string[];
  shortcut: string;
  max_results: number;
  is_setup_complete: boolean;
  semantic_enabled?: boolean;
  semantic_model?: string;
  semantic_f32_table?: boolean;
  visual_search_beta_enabled?: boolean;
  content_depth?: string;
  content_max_file_size_mb?: number;
  content_max_text_bytes?: number;
  content_csv_head_rows?: number;
  content_max_semantic_units?: number;
  crawl_mode?: "full" | "balanced" | "eco";
  pause_indexing?: boolean;
  mcp_allow_reindex?: boolean;
  extractors?: ExternalExtractor[];
  skip_hidden?: boolean;
  ocr_enabled?: boolean;
  ocr_max_image_bytes?: number;
  /** Tesseract language codes ("eng+Hindi" -> ["eng", "hin"]). Empty = auto. */
  ocr_languages?: string[];
  ocr_engine?: OcrEngine;
  app_mode?: "launcher" | "app";
  /** Closing the window hides to the tray instead of quitting. */
  close_to_background?: boolean;
  theme?: "light" | "dark";
  language?: string | null;
  aliases?: Alias[];
  saved_searches?: SavedSearch[];
  pins?: string[];
  config_version?: number;
  /** When non-empty, agent results and reads are scoped to these roots. */
  agent_scoped_roots?: string[];
  /** Include .gitignore/.cursorignore-matched files in the index. */
  include_ignored?: boolean;
  /** Show generated thumbnails on result cards (default on). */
  show_thumbnails?: boolean;
  /** Result layout. */
  results_view?: "grid" | "list";
  /** Automatically check for updates on startup and in the background. */
  auto_update?: boolean;
}

/* ---------------- Local LLM ---------------- */

export interface CompiledQuery {
  query: string;
  text: string;
  filters: string[];
  sort?: string | null;
  chips: InterpretationChip[];
}

export interface LlmCompileOutcome {
  status: "compiled" | "deterministic" | "unavailable" | "off";
  compiled?: CompiledQuery | null;
  detail?: string | null;
}

export interface LlmStatus {
  configured: boolean;
  reachable: boolean;
  provider: string;
  base_url: string;
  model: string;
  models: string[];
  cloud: boolean;
  error?: string | null;
}

export interface AskSource {
  index: number;
  path: string;
  line_start: number;
  line_end: number;
  snippet: string;
  score: number;
  cited: boolean;
}

export interface AskResponse {
  question: string;
  answer: string;
  sources: AskSource[];
  model: string;
  provider: string;
  endpoint: string;
  cloud: boolean;
  answered: boolean;
  as_of: number;
}

/* ---------------- One-click agent connect ---------------- */

export interface ClientStatus {
  id: string;
  name: string;
  config_path: string | null;
  installed: boolean;
  configured: boolean;
}

export interface InstallOutcome {
  status: ClientStatus;
  wrote: boolean;
  backup: string | null;
  message: string;
}

/* ---- OCR (get_ocr_status / download_ocr_language). The backend may ---- */
/* ---- not expose these yet; consumers must degrade to "unknown".     ---- */

export type OcrEngine = "tesseract" | "accurate" | "indra";

export interface OcrStatus {
  engine_present: boolean;
  languages: string[];
  /** Engine present AND at least one language data file on disk. */
  available: boolean;
  tessdata_dir: string;
  error: string | null;
  engine?: OcrEngine;
  /** PP-OCR can read: pack + runtime present and its self-test read known text. */
  accurate_available?: boolean;
  accurate_compiled?: boolean;
  /** Why PP-OCR cannot read right now, or null when it can. */
  accurate_problem?: string | null;
  /** Indra can read: pack + runtime present and its self-test read known text. */
  indra_available?: boolean;
  /** Why Indra cannot read right now, or null when it can. */
  indra_problem?: string | null;
  /** The engine actually reading images now; can differ from `engine`. */
  active_engine?: OcrEngine | "none";
}

/* ---- SystemStats (get_stats). The backend may not expose this yet; ---- */
/* ---- all consumers must degrade gracefully to zeroed fallbacks.      ---- */

export interface QueryStats {
  total: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
  last_ms: number;
  history: number[];
}

export interface IndexStats {
  files: number;
  dirs: number;
  content_docs: number;
  semantic_units: number;
  metadata_db_bytes: number;
  tantivy_bytes: number;
  vector_bytes: number;
  last_reconcile_secs_ago: number | null;
}

export type IngestPhase =
  | "idle"
  | "walking"
  | "extracting"
  | "embedding"
  | "committing";

export interface IngestionStats {
  is_indexing: boolean;
  phase: IngestPhase;
  files_indexed: number;
  content_indexed: number;
  units_indexed: number;
  files_per_sec: number;
  eta_secs: number | null;
  embed_queue_depth: number;
}

export interface RuntimeStats {
  uptime_secs: number;
  rss_bytes: number;
  watcher_ok: boolean;
  semantic_ready: boolean;
  model_id: string;
}

export interface SystemStats {
  queries: QueryStats;
  index: IndexStats;
  ingestion: IngestionStats;
  runtime: RuntimeStats;
}

/* ---- Model files (get_models_status / download_models / delete_models). ---- */
/* ---- The backend may not expose these yet; consumers degrade gracefully. ---- */

export interface ModelArtifact {
  name: string;
  url: string;
  bytes_expected: number;
  bytes_on_disk: number;
  present: boolean;
}

export interface ModelProgress {
  file: string;
  downloaded: number;
  total: number;
  phase: "verifying" | "downloading" | string;
}

export interface ModelsStatus {
  dir: string;
  ready: boolean;
  downloading: boolean;
  progress: ModelProgress | null;
  error: string | null;
  artifacts: ModelArtifact[];
  total_bytes: number;
  total_present_bytes: number;
}

/* ---- Doctor (run_doctor). One-shot diagnostics; never auto-blocks UI. ---- */

/** A repair the Health page can run for a failing check. */
export interface DoctorAction {
  /** `download-models`, `download-ocr-pack`, `download-indra-pack`,
   *  `restart-watcher`, `cleanup-storage`, `reindex`, `open-crash-log`. */
  kind: string;
  label: string;
  /** Settings section to open, for kinds that navigate instead of acting. */
  section?: string | null;
}

export interface DoctorCheck {
  id: string;
  label: string;
  status: "ok" | "warn" | "fail";
  detail: string;
  fix: string | null;
  action?: DoctorAction | null;
}

export interface DoctorReport {
  checks: DoctorCheck[];
  ran_at: number;
  summary: { ok: number; warn: number; fail: number };
}

/** Fully quantified, safe-to-render fallback for when `get_stats` is missing. */
export function emptyStats(): SystemStats {
  return {
    queries: { total: 0, p50_ms: 0, p95_ms: 0, p99_ms: 0, last_ms: 0, history: [] },
    index: {
      files: 0,
      dirs: 0,
      content_docs: 0,
      semantic_units: 0,
      metadata_db_bytes: 0,
      tantivy_bytes: 0,
      vector_bytes: 0,
      last_reconcile_secs_ago: null,
    },
    ingestion: {
      is_indexing: false,
      phase: "idle",
      files_indexed: 0,
      content_indexed: 0,
      units_indexed: 0,
      files_per_sec: 0,
      eta_secs: null,
      embed_queue_depth: 0,
    },
    runtime: {
      uptime_secs: 0,
      rss_bytes: 0,
      watcher_ok: false,
      semantic_ready: false,
      model_id: "",
    },
  };
}

/* ---------------- Auto Updater ---------------- */

export interface UpdateInfo {
  current_version: string;
  latest_version: string;
  has_update: boolean;
  release_name: string;
  release_notes: string;
  release_url: string;
  published_at: string;
  asset_name?: string | null;
  asset_url?: string | null;
  asset_size?: number | null;
}

export interface UpdateProgress {
  phase: "downloading" | "verifying" | "ready" | "error";
  downloaded: number;
  total: number;
  percent: number;
}

export interface UpdateStatus {
  busy: boolean;
  progress?: UpdateProgress | null;
  error?: string | null;
  downloaded_path?: string | null;
  ready_to_install: boolean;
}
