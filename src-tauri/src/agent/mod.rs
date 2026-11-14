//! Headless access to a local Kensaku index.
//!
//! This module powers the `kensaku-cli` binary and the MCP server. It only
//! reads the index built by the GUI: there is no in-app AI, no LLM call and no
//! network access anywhere on this path.

pub mod connect;
pub mod http;
pub mod mcp;
pub mod symbols;

use crate::config::KensakuConfig;
use crate::embed::{ModelRegistry, UnitStore};
use crate::indexer::content::ContentIndex;
use crate::indexer::metadata::MetadataStore;
use crate::indexer::metadata_db::MetadataDb;
use crate::search::{SearchDispatcher, SearchResponse};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Error returned whenever the GUI has not built an index yet.
pub const NO_INDEX_MESSAGE: &str =
    "No Kensaku index found. Launch Kensaku once to build the index.";

/// A process-lifetime snapshot of the Kensaku data directory.
///
/// Opening is the expensive part (SQLite load + Tantivy reader + embedding
/// table); [`Self::search`] and [`Self::state`] are cheap afterwards. Long-lived
/// transports (MCP) should open once per session.
pub struct HeadlessIndex {
    pub config: KensakuConfig,
    pub metadata: MetadataStore,
    pub content: Option<ContentIndex>,
    pub units: UnitStore,
    pub visual: crate::embed::visual::VisualStore,
    pub model: std::sync::Arc<ModelRegistry>,
    dir: PathBuf,
}

impl HeadlessIndex {
    /// Opens the user's real Kensaku data directory.
    pub fn open() -> Result<Self, String> {
        let dir = KensakuConfig::config_dir();
        if !dir.join("config.json").exists() && !dir.join("metadata.db").exists() {
            return Err(NO_INDEX_MESSAGE.to_string());
        }
        Self::open_at(&dir)
    }

    /// Opens an explicit data directory. Used by tests and reusable by callers
    /// that keep the index somewhere other than the default config dir.
    pub fn open_at(dir: &Path) -> Result<Self, String> {
        let config_path = dir.join("config.json");
        let db_path = dir.join("metadata.db");

        if !db_path.exists() {
            return Err(NO_INDEX_MESSAGE.to_string());
        }

        let config = if config_path.exists() {
            let raw = std::fs::read_to_string(&config_path)
                .map_err(|e| format!("Could not read config {}: {}", config_path.display(), e))?;
            KensakuConfig::load_from_str(&raw)
        } else {
            KensakuConfig::default()
        };

        // The headless surface must honor the same OCR setting as the GUI.
        crate::indexer::extract::set_ocr_enabled(config.ocr_enabled);
        crate::indexer::extract::image::set_ocr_max_image_bytes(config.ocr_max_image_bytes);
        crate::indexer::extract::set_content_caps(
            config.content_max_file_size_mb,
            config.content_max_text_bytes,
            config.content_csv_head_rows,
        );
        crate::embed::set_max_semantic_units(config.content_max_semantic_units);
        crate::ocr::set_engine(&config.ocr_engine);
        crate::indexer::extract::set_external_extractors(config.extractors.clone());

        // Read-write is fine: WAL lets the GUI writer and this connection
        // coexist. Nothing on the headless path writes.
        let db = MetadataDb::open_in_dir(dir)?;
        let metadata = MetadataStore::with_db(db);

        let model = ModelRegistry::global();
        let mut units = UnitStore::new(model.dim);
        let mut visual = crate::embed::visual::VisualStore::default();
        if let Some(db) = metadata.db() {
            match db.load_all_units() {
                Ok(loaded) => {
                    for (unit, vector) in loaded {
                        units.add_unit(unit, &vector);
                    }
                }
                Err(e) => eprintln!("[Kensaku] Failed to load semantic units: {}", e),
            }
            let _ = db.load_visual_units_into(&mut visual);
        }

        let index_dir = dir.join("index");
        let content = if index_dir.join("meta.json").exists() {
            Some(ContentIndex::open_read_only(&index_dir)?)
        } else {
            None
        };

        Ok(Self {
            config,
            metadata,
            content,
            units,
            visual,
            model,
            dir: dir.to_path_buf(),
        })
    }

    /// Runs a query against the loaded index.
    pub fn search(&self, query: &str, limit: usize) -> Result<SearchResponse, String> {
        self.search_page(query, limit, 0)
    }

    /// Runs a query with a stable result offset (MCP pagination).
    pub fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResponse, String> {
        if let Ok(value) = crate::daemon::request(
            "search",
            serde_json::json!({
                "query": query,
                "limit": limit,
                "offset": offset,
                "snippets": true
            }),
        ) && let Ok(response) = serde_json::from_value::<SearchResponse>(value)
        {
            return Ok(response);
        }
        self.search_local_page_with_snippets(query, limit, offset, true)
    }

    pub fn search_local(&self, query: &str, limit: usize) -> Result<SearchResponse, String> {
        self.search_local_page_with_snippets(query, limit, 0, true)
    }

    /// No-snippet local path used by MCP, which batches its own extraction.
    pub fn search_local_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResponse, String> {
        self.search_local_page_with_snippets(query, limit, offset, false)
    }

    pub fn search_local_page_with_snippets(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
        with_snippets: bool,
    ) -> Result<SearchResponse, String> {
        let aliases = self.config.alias_pairs();
        Ok(SearchDispatcher::search_response_fast_sorted_with_snippets(
            query,
            &self.metadata,
            &self.content,
            Some(&self.units),
            self.config.semantic_enabled,
            limit,
            0,
            crate::search::SortOrder::Best,
            &aliases,
            offset,
            with_snippets,
        ))
    }

    /// Computes snippets for `paths` in parallel on the extraction-free fast
    /// path (plain text is read directly, documents only hit the extraction
    /// cache). The result is aligned with `paths`.
    pub fn snippets(&self, paths: &[String], query: &str) -> Vec<Option<String>> {
        snippets_for_paths(paths, query)
    }

    /// Looks up visually similar images in the local visual store.
    pub fn find_similar_images_local(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<crate::embed::visual::SimilarImageResult>, String> {
        let p = Path::new(path);
        let (source_node_id, query_vec) = if let Some(entry) = self.metadata.get(p) {
            let vec_opt = self.visual.get_vector(entry.id).map(|v| v.to_vec());
            (Some(entry.id), vec_opt)
        } else {
            (None, None)
        };

        let query_vec = match query_vec {
            Some(v) => v,
            None => {
                let thumbs_dir = self.dir.join("thumbs");
                let thumb = crate::indexer::extract::image::thumbnail_path(p, &thumbs_dir);
                let encoder = crate::embed::visual::MockVisualEncoder;
                use crate::embed::visual::VisualEncoder;
                if thumb.is_file()
                    && let Some(v) = encoder.embed_image_from_thumbnail(&thumb)
                {
                    v
                } else if p.is_file() {
                    let img = image::open(p)
                        .map_err(|e| format!("Failed to open image {}: {}", p.display(), e))?;
                    encoder.embed_image(&img)
                } else {
                    return Err(format!("Image not found or not readable: {}", p.display()));
                }
            }
        };

        let floor = 0.30f32;
        let hits = self
            .visual
            .find_similar(&query_vec, source_node_id, floor, limit);

        let mut results = Vec::new();
        for (node_id, similarity) in hits {
            if let Some(entry) = self.metadata.get_by_node_id(node_id) {
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

    /// Health snapshot without a daemon: the local stores were opened at
    /// startup, so this is cheap.
    pub fn health_local(&self) -> Value {
        let state = match self.state_local() {
            Ok(state) => state,
            Err(error) => {
                return json!({
                    "ok": false,
                    "daemon": false,
                    "db": false,
                    "index": false,
                    "model": false,
                    "error": error,
                });
            }
        };
        json!({
            "ok": true,
            "daemon": false,
            "protocol_version": null,
            "watcher_ok": false,
            "db": self.dir.join("metadata.db").exists(),
            "index": self.content.is_some(),
            "model": state["active_model_id"].as_u64().is_some(),
            "state": state,
        })
    }

    /// Compact index status shared by `kensaku-cli state` and the MCP
    /// `index_state` tool.
    pub fn state(&self) -> Result<Value, String> {
        if let Ok(value) = crate::daemon::request("index_state", json!({})) {
            return Ok(value);
        }
        self.state_local()
    }

    pub fn state_local(&self) -> Result<Value, String> {
        let mut files = 0u64;
        let mut dirs = 0u64;
        for i in 0..self.metadata.len() {
            if self.metadata.node(i as u32).is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
        }

        let content_docs = self.content.as_ref().map(|c| c.num_docs()).unwrap_or(0);
        let index_bytes = dir_size_bytes(&self.dir.join("index"))
            .saturating_add(prefixed_file_bytes(&self.dir, "metadata.db"));
        let last_reconcile = last_modified_secs(&[
            self.dir.join("metadata.db"),
            self.dir.join("index").join("meta.json"),
        ]);
        let roots: Vec<String> = self
            .config
            .watched_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        // The units' model id is what the stored vectors were encoded with;
        // the active id is what a new query is embedded with. They can differ
        // until the GUI re-embeds after a model change.
        let units_model_id = match self.metadata.db() {
            Some(db) => db.units_model_id()?,
            None => None,
        };

        let model_dir = self.dir.join("models");
        let model_real = model_dir.join("model.safetensors").is_file()
            && model_dir.join("tokenizer.json").is_file();

        Ok(json!({
            "files": files,
            "dirs": dirs,
            "content_docs": content_docs,
            "semantic_units": self.units.len() as u64,
            "index_bytes": index_bytes,
            "model_id": units_model_id,
            "active_model_id": self.model.model_id,
            "model_name": self.model.name,
            "model_real": model_real,
            "roots": roots,
            "last_reconcile": last_reconcile,
        }))
    }
}

/// Computes extraction-free snippets for a page of results.
pub fn snippets_for_paths(paths: &[String], query: &str) -> Vec<Option<String>> {
    use rayon::prelude::*;
    paths
        .par_iter()
        .map(|path| crate::indexer::content::snippet_for_path(Path::new(path), query))
        .collect()
}

/// Backend selected for one MCP session.
///
/// `Daemon` never opens SQLite, Tantivy or the embedding table in this
/// process; tool calls are forwarded to the single writer. `Local` is the
/// read-only fallback used when no daemon is running. `None` still answers
/// `initialize` and returns actionable errors from tools.
pub enum AgentSession<'a> {
    Daemon,
    Local(&'a HeadlessIndex),
    None,
}

impl AgentSession<'_> {
    pub fn is_daemon(&self) -> bool {
        matches!(self, Self::Daemon)
    }

    /// Loads the security policy for this session.
    ///
    /// This fails closed: a daemon that cannot answer `get_config`, a
    /// malformed response, or a session with no index all return an error. The
    /// old accessors turned those failures into empty vectors, and an empty
    /// root list means "the whole index is visible", so a config hiccup
    /// silently widened the agent's scope.
    pub fn policy(&self) -> Result<AgentPolicy, String> {
        let config = match self {
            Self::Daemon => crate::daemon::request("get_config", json!({}))
                .map_err(|e| format!("agent policy unavailable: {e}"))
                .and_then(|value| {
                    serde_json::from_value::<KensakuConfig>(value)
                        .map_err(|e| format!("agent policy unavailable: {e}"))
                })?,
            Self::Local(index) => index.config.clone(),
            Self::None => return Err(NO_INDEX_MESSAGE.to_string()),
        };
        Ok(AgentPolicy {
            roots: config.agent_scoped_roots,
            excluded_extensions: config.excluded_extensions,
        })
    }

    /// Whether `path` is present in the index this session serves.
    ///
    /// Direct reads are restricted to indexed files: an MCP client must not be
    /// able to turn `read_snippet` into an arbitrary-file reader by naming a
    /// path the indexer deliberately skipped (secrets, excluded extensions,
    /// files outside every watched root).
    pub fn contains_path(&self, path: &str) -> Result<bool, String> {
        let requested = Path::new(path);
        let canonical = std::fs::canonicalize(requested).ok();
        match self {
            Self::Daemon => {
                let value = crate::daemon::request("contains_path", json!({ "path": path }))?;
                value
                    .get("indexed")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| "invalid contains_path response".to_string())
            }
            Self::Local(index) => Ok(index.metadata.get(requested).is_some()
                || canonical
                    .as_deref()
                    .is_some_and(|p| index.metadata.get(p).is_some())),
            Self::None => Err(NO_INDEX_MESSAGE.to_string()),
        }
    }

    pub fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResponse, String> {
        match self {
            Self::Daemon => {
                let value = crate::daemon::request(
                    "search",
                    json!({
                        "query": query,
                        "limit": limit,
                        "offset": offset,
                        "snippets": false
                    }),
                )?;
                serde_json::from_value(value).map_err(|e| e.to_string())
            }
            Self::Local(index) => index.search_local_page(query, limit, offset),
            Self::None => Err(NO_INDEX_MESSAGE.to_string()),
        }
    }

    /// Keyset-paginated exhaustive search: matches strictly after
    /// `start_after` (a path), for pagination past the 10k exhaustive cap.
    pub fn search_page_after(
        &self,
        query: &str,
        limit: usize,
        start_after: &str,
    ) -> Result<SearchResponse, String> {
        match self {
            Self::Daemon => {
                let value = crate::daemon::request(
                    "search",
                    json!({
                        "query": query,
                        "limit": limit,
                        "offset": 0,
                        "snippets": false,
                        "start_after": start_after,
                        "mode": "exhaustive",
                    }),
                )?;
                serde_json::from_value(value).map_err(|e| e.to_string())
            }
            Self::Local(index) => {
                // Callers pass a composed query that already carries
                // `mode:exhaustive`; the keyset filter does the rest.
                Ok(SearchDispatcher::search_response_exhaustive_after(
                    query.trim(),
                    &index.metadata,
                    &index.content,
                    Some(&index.units),
                    index.config.semantic_enabled,
                    limit,
                    0,
                    &index.config.alias_pairs(),
                    start_after,
                    false,
                ))
            }
            Self::None => Err(NO_INDEX_MESSAGE.to_string()),
        }
    }

    pub fn state(&self) -> Result<Value, String> {
        match self {
            Self::Daemon => crate::daemon::request("index_state", json!({})),
            Self::Local(index) => index.state_local(),
            Self::None => Err(NO_INDEX_MESSAGE.to_string()),
        }
    }

    pub fn health(&self) -> Value {
        match self {
            Self::Daemon => crate::daemon::request("health", json!({})).unwrap_or_else(|error| {
                json!({
                    "ok": false,
                    "daemon": true,
                    "db": false,
                    "index": false,
                    "model": false,
                    "error": error,
                })
            }),
            Self::Local(index) => index.health_local(),
            Self::None => json!({
                "ok": false,
                "daemon": false,
                "db": false,
                "index": false,
                "model": false,
                "error": NO_INDEX_MESSAGE,
            }),
        }
    }

    pub fn snippets(&self, paths: &[String], query: &str) -> Vec<Option<String>> {
        snippets_for_paths(paths, query)
    }

    /// Looks up visually similar images.
    pub fn find_similar_images(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<crate::embed::visual::SimilarImageResult>, String> {
        match self {
            Self::Daemon => {
                let value = crate::daemon::request(
                    "find_similar_images",
                    json!({
                        "path": path,
                        "limit": limit,
                    }),
                )?;
                serde_json::from_value(value).map_err(|e| e.to_string())
            }
            Self::Local(index) => index.find_similar_images_local(path, limit),
            Self::None => Err(NO_INDEX_MESSAGE.to_string()),
        }
    }

    /// `reindex` is the only write tool. It requires both the config opt-in
    /// and a running daemon, so read-only fallback can never write.
    pub fn reindex(&self) -> Result<Value, String> {
        match self {
            Self::Daemon => crate::daemon::request("reindex", json!({})),
            Self::Local(_) => Err(
                "reindex requires the Kensaku daemon; start it before enabling this tool"
                    .to_string(),
            ),
            Self::None => Err(NO_INDEX_MESSAGE.to_string()),
        }
    }
}

/// The security policy an agent session must apply to every read.
///
/// Built by [`AgentSession::policy`], which fails closed. Keeping the two
/// values together makes it hard to apply one (scope) without the other
/// (excluded extensions).
#[derive(Debug, Clone, Default)]
pub struct AgentPolicy {
    pub roots: Vec<String>,
    pub excluded_extensions: Vec<String>,
}

impl AgentPolicy {
    /// Rejects caller-supplied `in:` scopes that could widen the agent scope.
    ///
    /// The parser ORs every include scope together, so a caller query such as
    /// `in:"/etc"` plus the roots appended by [`compose_query_scoped`] searched
    /// both `/etc` and the roots. When roots are configured, every caller
    /// include must be inside one of them. Negated scopes are allowed because
    /// they only narrow the result set.
    pub fn validate_query_scope(&self, raw_query: &str) -> Result<(), String> {
        if self.roots.is_empty() {
            return Ok(());
        }
        for token in crate::query::lexer::tokenize(raw_query) {
            if let crate::query::lexer::Token::KeyValue {
                negated: false,
                key,
                value,
                ..
            } = token
                && key.eq_ignore_ascii_case("in")
                && !self
                    .roots
                    .iter()
                    .any(|root| scope_contains(root, value.trim()))
            {
                return Err(format!(
                    "query scope 'in:{value}' is outside the configured agent scope; use the 'path' parameter instead"
                ));
            }
        }
        Ok(())
    }

    pub fn path_allowed(&self, path: &str) -> bool {
        agent_path_allowed(&self.roots, path)
    }

    pub fn read_allowed(&self, path: &str) -> bool {
        agent_read_allowed(&self.roots, &self.excluded_extensions, path)
    }

    /// Scope paths to compose into a query for an optional requested path.
    pub fn query_scopes(&self, requested: Option<&str>) -> Result<Vec<String>, String> {
        agent_scope_paths(&self.roots, requested)
    }
}

/// Applies the configured agent-scope roots to an optional requested path.
///
/// - No roots configured: pass the request through unchanged.
/// - Roots configured: a requested path must live under one of them (else
///   the request is rejected); an absent path defaults to the union of the
///   roots.
///
/// Containment is decided on resolved paths: existing paths are canonicalized
/// (which resolves `..`, `.` and symlinks) and non-existent paths fall back to
/// a lexical normalization, so a request cannot escape a root with `..`.
///
/// Returns the scope paths to append as `in:"..."` tokens, or an error for
/// out-of-scope requests.
pub fn agent_scope_paths(roots: &[String], requested: Option<&str>) -> Result<Vec<String>, String> {
    if roots.is_empty() {
        return Ok(requested
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| vec![normalize_scope_path(path)])
            .unwrap_or_default());
    }

    let requested = requested.map(str::trim).filter(|path| !path.is_empty());
    if let Some(path) = requested {
        if roots.iter().any(|root| scope_contains(root, path)) {
            // Keep the caller's spelling for query matching; containment was
            // already decided on the resolved path.
            return Ok(vec![normalize_scope_path(path)]);
        }
        return Err(format!(
            "path is outside the configured agent scope: {}",
            path
        ));
    }
    Ok(roots
        .iter()
        .map(|root| normalize_scope_path(root))
        .collect())
}

/// Expands a leading `~` and lexically normalizes `.`/`..` components.
///
/// This is for display/composition only; use [`scope_contains`] for
/// containment checks so symlinks are resolved where possible.
fn normalize_scope_path(path: &str) -> String {
    lexical_scope_path(&expand_tilde(path))
        .to_string_lossy()
        .into_owned()
}

fn expand_tilde(path: &str) -> String {
    let path = path.trim();
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    if !rest.is_empty() && !rest.starts_with('/') && !rest.starts_with('\\') {
        // `~user` is not supported; leave it untouched rather than
        // concatenating it onto the current home directory.
        return path.to_string();
    }
    let Some(home) =
        std::env::var_os("HOME").or_else(|| dirs::home_dir().map(|p| p.into_os_string()))
    else {
        return path.to_string();
    };
    let mut expanded = home.to_string_lossy().into_owned();
    if !rest.is_empty() {
        expanded.push_str(rest);
    }
    expanded
}

fn lexical_scope_path(path: &str) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
        }
    }
    out
}

/// Resolves a path for scope comparison. Existing paths are canonicalized so
/// symlinks and `..` resolve exactly as the OS would; non-existent paths use a
/// lexical normalization (enough to stop `..` escapes).
fn resolved_scope_path(path: &str) -> PathBuf {
    let expanded = expand_tilde(path);
    std::fs::canonicalize(&expanded).unwrap_or_else(|_| lexical_scope_path(&expanded))
}

fn scope_contains(root: &str, path: &str) -> bool {
    let root = resolved_scope_path(root);
    let path = resolved_scope_path(path);
    path == root || path.starts_with(&root)
}

/// True when `path` is within the configured agent-scope roots (or no roots
/// are configured). Used to gate `read_snippet`-style single-path reads.
pub fn agent_path_allowed(roots: &[String], path: &str) -> bool {
    if roots.is_empty() {
        return true;
    }
    roots.iter().any(|root| scope_contains(root, path))
}

/// Extensions a direct agent read must never return, whatever the scope says.
///
/// Indexing already excludes these by default (private keys, certificates,
/// password databases); a single-path read must not be a way around that.
const ALWAYS_DENIED_EXTENSIONS: &[&str] = &[
    "pem", "key", "pfx", "p12", "crt", "cer", "kdbx", "gpg", "asc", "jks", "keystore",
];

/// Filenames that carry credentials regardless of extension.
///
/// The list intentionally covers the common credential stores agents have been
/// tricked into reading. `.env` is matched as a prefix too, so `.env.local`,
/// `.env.production` and similar variants are covered.
const ALWAYS_DENIED_NAMES: &[&str] = &[
    ".env",
    ".netrc",
    ".pgpass",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".htpasswd",
    ".dockercfg",
    ".gitconfig",
    ".kubeconfig",
    ".my.cnf",
    "credentials",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "known_hosts",
    "secring.gpg",
    "auth.json",
];

/// Path components that hold credential stores. A direct read must never
/// traverse these even when the file name looks harmless.
const ALWAYS_DENIED_COMPONENTS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".docker",
    ".kube",
    ".password-store",
];

/// Whether the agent surface may read `path` directly.
///
/// Scope answers "where"; this answers "what". An empty scope means the whole
/// index is searchable, which is the documented default, but it must not also
/// mean "and any file on disk": without this check `read_snippet` on an
/// absolute path returned the contents of `~/.ssh/id_ed25519` to any connected
/// MCP client, because a direct read never consults the indexing denylist.
///
/// Callers must additionally require that the path is indexed
/// ([`AgentSession::contains_path`]); this function only answers whether the
/// name and scope permit a read.
pub fn agent_read_allowed(roots: &[String], excluded_extensions: &[String], path: &str) -> bool {
    if !agent_path_allowed(roots, path) {
        return false;
    }

    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower_name = file_name.to_ascii_lowercase();
    if lower_name == ".env"
        || lower_name.starts_with(".env.")
        || ALWAYS_DENIED_NAMES.contains(&lower_name.as_str())
    {
        return false;
    }

    if path.split(['/', '\\']).any(|component| {
        let lower = component.to_ascii_lowercase();
        ALWAYS_DENIED_COMPONENTS.contains(&lower.as_str())
    }) {
        return false;
    }

    let extension = lower_name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or_default();
    extension.is_empty()
        || !(ALWAYS_DENIED_EXTENSIONS.contains(&extension)
            || excluded_extensions.iter().any(|excluded| {
                excluded
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(extension)
            }))
}

/// Composes a Kensaku query from a base query plus one or more scope paths.
///
/// Like [`compose_query`], but every scope path becomes an `in:"..."` token
/// (multiple include scopes are OR'd by the parser — the agent-scope union).
pub fn compose_query_scoped(
    base: &str,
    scope_paths: &[String],
    ext: Option<&str>,
    modified_after: Option<&str>,
    exhaustive: bool,
) -> String {
    let (first, rest) = match scope_paths.split_first() {
        Some((first, rest)) => (Some(first.as_str()), rest),
        None => (None, &[][..]),
    };
    let mut composed = compose_query(base, first, ext, modified_after, exhaustive);
    for path in rest {
        if !composed.is_empty() {
            composed.push(' ');
        }
        composed.push_str(&format!("in:\"{}\"", escape_query_value(path)));
    }
    composed
}

/// Composes a Kensaku query from a base query plus optional agent filters.
///
/// Paths are quoted so a path containing spaces (or a `:` / `"`) survives the
/// query lexer; embedded quotes and backslashes are escaped.
pub fn compose_query(
    base: &str,
    path: Option<&str>,
    ext: Option<&str>,
    modified_after: Option<&str>,
    exhaustive: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let base = base.trim();
    if !base.is_empty() {
        parts.push(base.to_string());
    }

    if let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) {
        parts.push(format!("in:\"{}\"", escape_query_value(path)));
    }

    if let Some(ext) = ext.map(str::trim).filter(|e| !e.is_empty()) {
        let ext = ext.trim_start_matches('.');
        if !ext.is_empty() {
            parts.push(format!("ext:{}", ext));
        }
    }

    if let Some(modified) = modified_after.map(str::trim).filter(|m| !m.is_empty()) {
        parts.push(format!("modified:{}", modified));
    }

    if exhaustive {
        parts.push("mode:exhaustive".to_string());
    }

    parts.join(" ")
}

fn escape_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Truncates `text` to at most `max_chars` characters without splitting a
/// UTF-8 code point.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// Random-enough token for a loopback-only local socket or HTTP endpoint.
///
/// Not cryptographic: the threat model is other local processes, and the
/// token file is 0600. Same scheme the daemon uses for its socket token.
pub fn new_connection_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}", std::process::id(), nanos)
}

/// Unix timestamp (seconds) used for `as_of` / freshness fields.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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

/// Sums files in `dir` whose name starts with `prefix` (e.g. `metadata.db`,
/// `metadata.db-wal`, `metadata.db-shm`).
fn prefixed_file_bytes(dir: &Path, prefix: &str) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(prefix) {
            continue;
        }
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
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
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
        })
        .max()
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kensaku-scope-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("inside.txt"), "x").unwrap();
        std::fs::write(dir.join("sub/inside.txt"), "x").unwrap();
        std::fs::write(dir.with_extension("sibling"), "x").unwrap();
        dir
    }

    #[test]
    fn direct_reads_refuse_credential_files() {
        // No scope configured is the documented default, and it means the
        // whole index is searchable. It must not also mean every file on disk
        // is readable: this is how #95-class "agent read anything" bugs get in.
        let roots: Vec<String> = Vec::new();
        let excluded: Vec<String> = vec!["db".to_string(), "sqlite".to_string()];

        assert!(agent_read_allowed(&roots, &excluded, "/home/u/notes.txt"));
        assert!(agent_read_allowed(
            &roots,
            &excluded,
            "/home/u/project/README.md"
        ));

        // Known credential filenames.
        assert!(!agent_read_allowed(
            &roots,
            &excluded,
            "/home/u/.ssh/id_ed25519"
        ));
        assert!(!agent_read_allowed(&roots, &excluded, "/home/u/id_rsa"));
        assert!(!agent_read_allowed(&roots, &excluded, "/srv/app/.env"));

        // Credential extensions, whatever the configured index excludes say.
        assert!(!agent_read_allowed(&roots, &[], "/home/u/certs/server.pem"));
        assert!(!agent_read_allowed(&roots, &[], "/home/u/private.KEY"));
        assert!(!agent_read_allowed(&roots, &[], "/home/u/client.pfx"));

        // Extensions the user excluded from the index are not readable either.
        assert!(!agent_read_allowed(&roots, &excluded, "/home/u/app.sqlite"));
        assert!(!agent_read_allowed(&roots, &excluded, "/home/u/state.db"));

        // Scope still wins when it is configured.
        let scoped = vec!["/home/u/project".to_string()];
        assert!(!agent_read_allowed(
            &scoped,
            &excluded,
            "/home/u/elsewhere/notes.txt"
        ));
    }

    #[test]
    fn accepts_paths_inside_a_root() {
        let root = temp_root("inside");
        let roots = vec![root.to_string_lossy().into_owned()];
        assert!(agent_path_allowed(
            &roots,
            &root.join("inside.txt").to_string_lossy()
        ));
        assert!(agent_path_allowed(
            &roots,
            &root.join("sub/inside.txt").to_string_lossy()
        ));
        assert_eq!(
            agent_scope_paths(&roots, Some(&root.join("sub").to_string_lossy())).unwrap(),
            vec![root.join("sub").to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn rejects_parent_dir_escape() {
        let root = temp_root("dotdot");
        let roots = vec![root.to_string_lossy().into_owned()];
        let escape = root.join("sub/../../outside.txt");
        let escape = escape.to_string_lossy().into_owned();
        assert!(!agent_path_allowed(&roots, &escape));
        assert!(agent_scope_paths(&roots, Some(&escape)).is_err());
    }

    #[test]
    fn rejects_sibling_prefix() {
        let root = temp_root("sibling");
        let roots = vec![root.to_string_lossy().into_owned()];
        let sibling = root.with_extension("sibling");
        assert!(!agent_path_allowed(&roots, &sibling.to_string_lossy()));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let root = temp_root("symlink");
        let outside = temp_root("symlink-outside");
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let roots = vec![root.to_string_lossy().into_owned()];
        let escaped = root.join("escape/inside.txt");
        assert!(!agent_path_allowed(&roots, &escaped.to_string_lossy()));
        assert!(agent_scope_paths(&roots, Some(&escaped.to_string_lossy())).is_err());
    }

    #[test]
    fn empty_roots_allow_anything() {
        assert!(agent_path_allowed(&[], "/etc/passwd"));
        // The normalization preserves POSIX roots, so with no configured
        // roots the request passes through lexically unchanged. Windows
        // renders the POSIX root with the native separator.
        let expected = if cfg!(windows) {
            r"\tmp\demo"
        } else {
            "/tmp/demo"
        };
        assert_eq!(
            agent_scope_paths(&[], Some("/tmp/demo")).unwrap(),
            vec![expected.to_string()]
        );
    }

    #[test]
    fn lexical_normalization_stays_above_root() {
        // Traversing above the root collides with the root in both cases,
        // but the fallback `..` for relative escapes is rendered with the
        // native separator on Windows.
        let (above_root, relative_escape) = if cfg!(windows) {
            (r"\", r"..\a")
        } else {
            ("/", "../a")
        };
        assert_eq!(lexical_scope_path("/a/../..").to_string_lossy(), above_root);
        assert_eq!(
            lexical_scope_path("../a").to_string_lossy(),
            relative_escape
        );
    }

    #[test]
    fn direct_reads_refuse_credential_variants() {
        let roots: Vec<String> = Vec::new();
        let excluded: Vec<String> = Vec::new();

        for path in [
            "/srv/app/.env.production",
            "/srv/app/.env.local",
            "/home/u/.git-credentials",
            "/home/u/.npmrc",
            "/home/u/.aws/credentials",
            "/home/u/.ssh/config",
            "/home/u/.docker/config.json",
            "/home/u/.kube/config",
            "/home/u/.gnupg/private-keys-v1.d/key",
        ] {
            assert!(
                !agent_read_allowed(&roots, &excluded, path),
                "{path} must not be readable by an agent"
            );
        }
    }

    #[test]
    fn query_scope_rejects_union_broadening() {
        let root = temp_root("query-scope");
        let root_string = root.to_string_lossy().into_owned();
        let policy = AgentPolicy {
            roots: vec![root_string.clone()],
            excluded_extensions: Vec::new(),
        };

        // With no roots the whole index is visible, so an `in:` scope is not
        // a privilege escalation.
        assert!(
            AgentPolicy::default()
                .validate_query_scope("in:\"/etc\" x")
                .is_ok()
        );

        // An include inside the root is allowed.
        let inside = root.join("sub").to_string_lossy().into_owned();
        assert!(
            policy
                .validate_query_scope(&format!("in:\"{inside}\" x"))
                .is_ok()
        );

        // An include outside the root is rejected; appending the roots to the
        // caller query would otherwise OR both scopes together.
        assert!(policy.validate_query_scope("in:\"/etc\" x").is_err());
        assert!(
            policy
                .validate_query_scope(&format!("in:\"{root_string}\" in:\"/etc\""))
                .is_err()
        );

        // Negated scopes only narrow, so they stay allowed.
        assert!(policy.validate_query_scope("x -in:\"/etc\"").is_ok());
    }
}
