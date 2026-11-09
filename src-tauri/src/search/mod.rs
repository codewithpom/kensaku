pub mod fuzzy;

use crate::embed::{ExactScanner, ModelRegistry, UnitStore};
use crate::indexer::content::{ContentFilters, ContentIndex, FieldKind};
use crate::indexer::metadata::MetadataStore;
use crate::indexer::walker::FileEntry;
use crate::query::ast::{ActiveFilterDto, Filter, Mode, TermField};
use crate::query::parse_query;
use crate::search::fuzzy::FuzzySearcher;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Hard cap for `mode:exhaustive` result enumeration.
const EXHAUSTIVE_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MatchType {
    FileName,
    Content,
    /// Found by the folded/trigram fallback after exact matches ran short.
    ContentFolded,
    Semantic,
    Visual,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub path: String,
    pub filename: String,
    pub snippet: Option<String>,
    pub score: f64,
    pub file_size: u64,
    pub modified: u64,
    pub is_dir: bool,
    pub match_type: MatchType,
    pub best_range: Option<(usize, usize)>,
    /// Number of files collapsed into this row by version grouping.
    #[serde(default = "one")]
    pub group_count: usize,
    /// Other paths in this version group (empty when not grouped).
    #[serde(default)]
    pub group_paths: Vec<String>,
}

fn one() -> usize {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Facets {
    pub all: usize,
    /// Category counts: file/folder/image/doc/code/media/archive.
    pub types: HashMap<String, usize>,
    /// Language family counts keyed by common extensions.
    pub languages: HashMap<String, usize>,
    /// Parent-directory name counts.
    pub places: HashMap<String, usize>,
    /// today / week / month / year / older.
    pub time: HashMap<String, usize>,
    /// tiny (<1 MB) / small (<10 MB) / medium (<100 MB) / large (>=100 MB).
    pub size: HashMap<String, usize>,
    /// Anything modified in the last 7 days.
    pub recent: usize,
}

const TYPE_BUCKETS: [&str; 7] = ["folder", "image", "doc", "code", "media", "archive", "file"];
const LANGUAGE_BUCKETS: [&str; 15] = [
    "rust",
    "typescript",
    "javascript",
    "python",
    "go",
    "java",
    "c",
    "cpp",
    "ruby",
    "php",
    "swift",
    "kotlin",
    "shell",
    "markdown",
    "config",
];
const TIME_BUCKETS: [&str; 5] = ["today", "week", "month", "year", "older"];
const SIZE_BUCKETS: [&str; 4] = ["tiny", "small", "medium", "large"];

/// Category and language index for one extension. Categories index
/// [`TYPE_BUCKETS`] (folders are 0 and handled by the caller); languages index
/// [`LANGUAGE_BUCKETS`]. Combining both lookups avoids a second string match on
/// the large-result-set facet path.
fn classify_extension(ext: &str) -> (usize, Option<usize>) {
    match ext {
        "rs" => (3, Some(0)),
        "ts" | "tsx" => (3, Some(1)),
        "js" | "jsx" => (3, Some(2)),
        "py" => (3, Some(3)),
        "go" => (3, Some(4)),
        "java" => (3, Some(5)),
        "c" => (3, Some(6)),
        "cpp" | "h" | "hpp" => (3, Some(7)),
        "rb" => (3, Some(8)),
        "php" => (3, Some(9)),
        "swift" => (3, Some(10)),
        "kt" => (3, Some(11)),
        "sh" | "bash" | "fish" => (3, Some(12)),
        "md" => (2, Some(13)),
        "json" | "yaml" | "yml" | "toml" => (3, Some(14)),

        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "tiff" | "tif"
        | "heic" | "avif" => (1, None),
        "pdf" | "txt" | "doc" | "docx" | "ppt" | "pptx" | "odp" | "key" | "xls" | "xlsx"
        | "csv" | "tsv" | "ods" | "rtf" | "odt" | "pages" | "epub" | "log" => (2, None),
        "lua" | "zig" | "nim" | "html" | "css" | "scss" | "xml" | "sql" | "conf" => (3, None),
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "mp3" | "flac" | "wav" | "ogg" | "m4a"
        | "aac" | "opus" => (4, None),
        "zip" | "tar" | "gz" | "rar" | "7z" | "bz2" | "xz" | "dmg" | "iso" => (5, None),
        _ => (6, None),
    }
}

/// FNV-1a, used only for the short-lived parent-name map. Place keys are
/// borrowed from the result set, so the map never allocates just to count.
#[derive(Default)]
struct FnvHasher(u64);

impl std::hash::Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = hash;
    }
}

/// Accumulator for facet counts. The parent-name keys borrow from the result
/// set, so counting never allocates per row.
#[derive(Default)]
struct FacetAcc<'a> {
    types: [usize; TYPE_BUCKETS.len()],
    languages: [usize; LANGUAGE_BUCKETS.len()],
    time: [usize; TIME_BUCKETS.len()],
    size: [usize; SIZE_BUCKETS.len()],
    recent: usize,
    places: HashMap<&'a str, usize, std::hash::BuildHasherDefault<FnvHasher>>,
}

impl<'a> FacetAcc<'a> {
    fn add(&mut self, result: &'a SearchResult, now_sec: u64) {
        let ext_owned;
        let ext = match result.filename.rsplit_once('.') {
            Some((_, ext)) if ext.bytes().any(|byte| byte.is_ascii_uppercase()) => {
                ext_owned = ext.to_ascii_lowercase();
                ext_owned.as_str()
            }
            Some((_, ext)) => ext,
            None => "",
        };
        let (type_index, language) = if result.is_dir {
            (0, None)
        } else {
            classify_extension(ext)
        };
        self.types[type_index] += 1;
        if let Some(language) = language {
            self.languages[language] += 1;
        }
        // Two splits are much cheaper than Path::parent/file_name for the
        // millions of rows a broad query can return.
        let mut components = result.path.rsplit(['/', '\\']);
        let _file = components.next();
        if let Some(parent_name) = components.next()
            && !parent_name.is_empty()
        {
            *self.places.entry(parent_name).or_insert(0) += 1;
        }
        let age = now_sec.saturating_sub(result.modified);
        let bucket = if age < 86_400 {
            0
        } else if age < 7 * 86_400 {
            1
        } else if age < 30 * 86_400 {
            2
        } else if age < 365 * 86_400 {
            3
        } else {
            4
        };
        self.time[bucket] += 1;
        if age < 7 * 86_400 {
            self.recent += 1;
        }
        let size_bucket = if result.file_size < 1_048_576 {
            0
        } else if result.file_size < 10 * 1_048_576 {
            1
        } else if result.file_size < 100 * 1_048_576 {
            2
        } else {
            3
        };
        self.size[size_bucket] += 1;
    }

    fn merge(mut self, other: Self) -> Self {
        for (left, right) in self.types.iter_mut().zip(other.types) {
            *left += right;
        }
        for (left, right) in self.languages.iter_mut().zip(other.languages) {
            *left += right;
        }
        for (left, right) in self.time.iter_mut().zip(other.time) {
            *left += right;
        }
        for (left, right) in self.size.iter_mut().zip(other.size) {
            *left += right;
        }
        self.recent += other.recent;
        for (name, count) in other.places {
            *self.places.entry(name).or_insert(0) += count;
        }
        self
    }

    fn into_facets(self, all: usize) -> Facets {
        let mut facets = Facets {
            all,
            recent: self.recent,
            ..Default::default()
        };
        for (index, count) in self.types.iter().enumerate() {
            if *count > 0 {
                facets.types.insert(TYPE_BUCKETS[index].to_string(), *count);
            }
        }
        for (index, count) in self.languages.iter().enumerate() {
            if *count > 0 {
                facets
                    .languages
                    .insert(LANGUAGE_BUCKETS[index].to_string(), *count);
            }
        }
        for (index, count) in self.time.iter().enumerate() {
            if *count > 0 {
                facets.time.insert(TIME_BUCKETS[index].to_string(), *count);
            }
        }
        for (index, count) in self.size.iter().enumerate() {
            if *count > 0 {
                facets.size.insert(SIZE_BUCKETS[index].to_string(), *count);
            }
        }
        facets.places = self
            .places
            .into_iter()
            .map(|(name, count)| (name.to_string(), count))
            .collect();
        facets
    }
}

/// Computes facet counts over the full matched set.
///
/// Public so the performance harness can measure it directly; the UI receives
/// it through [`SearchResponse::facets`]. Large matched sets are folded across
/// the existing rayon pool; small ones stay sequential to avoid scheduling
/// overhead on the keystroke path.
pub fn compute_facets(results: &[SearchResult], now_sec: u64) -> Facets {
    const PARALLEL_THRESHOLD: usize = 20_000;

    let accumulator = if results.len() >= PARALLEL_THRESHOLD {
        use rayon::prelude::*;
        results
            .par_iter()
            .fold(FacetAcc::default, |mut acc, result| {
                acc.add(result, now_sec);
                acc
            })
            .reduce(FacetAcc::default, FacetAcc::merge)
    } else {
        let mut acc = FacetAcc::default();
        for result in results {
            acc.add(result, now_sec);
        }
        acc
    };
    accumulator.into_facets(results.len())
}

/// Version grouping: collapse `resume`, `resume_v2`, `resume_final` into one
/// row when they share a parent directory, stem and extension. Conservative:
/// at least one member must carry a version marker.
fn group_versions(results: &mut Vec<SearchResult>) {
    use std::sync::OnceLock;
    static MARKERS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let markers = MARKERS.get_or_init(|| {
        [
            r"(?i)[ _.\-]*(?:v|version)[ _.\-]*\d+$",
            r"(?i)[ _.\-]*(?:final|final[ _.\-]*final|copy|backup|old|new)(?:[ _.\-]*\d+)?$",
            r"(?i)[ _.\-]*\(\d+\)$",
            r"(?i)[ _.\-]*-?[ _.\-]*copy$",
        ]
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect()
    });

    let mut groups: HashMap<(String, String, String), Vec<usize>> = HashMap::new();
    let mut has_marker: HashMap<(String, String, String), bool> = HashMap::new();
    for (i, r) in results.iter().enumerate() {
        if r.is_dir || r.group_count > 1 {
            continue;
        }
        let (stem, ext) = match r.filename.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), e.to_lowercase()),
            None => (r.filename.clone(), String::new()),
        };
        let parent = std::path::Path::new(&r.path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut base = stem.to_lowercase();
        let mut marker = false;
        for re in markers {
            let next = re.replace(&base, "").to_string();
            if next != base {
                marker = true;
                base = next;
            }
        }
        let base = base
            .trim_matches(|c: char| c == ' ' || c == '_' || c == '-' || c == '.')
            .to_string();
        if base.is_empty() {
            continue;
        }
        let key = (parent, base, ext);
        groups.entry(key.clone()).or_default().push(i);
        if marker {
            has_marker.insert(key, true);
        }
    }

    let mut remove = HashSet::new();
    for (key, indices) in groups {
        if indices.len() < 2 || !has_marker.get(&key).copied().unwrap_or(false) {
            continue;
        }
        let keep = indices[0];
        let others: Vec<String> = indices[1..]
            .iter()
            .map(|&i| results[i].path.clone())
            .collect();
        results[keep].group_count = indices.len();
        results[keep].group_paths = others;
        for &i in &indices[1..] {
            remove.insert(i);
        }
    }
    let mut i = 0;
    results.retain(|_| {
        let keep = !remove.contains(&i);
        i += 1;
        keep
    });
}

/// Collapses image results that belong to the same screenshot burst.
///
/// The first result in rank order is kept and carries the other paths; this is
/// a pure O(results) pass over an in-memory map, so it adds no database query
/// to the search path. Non-image rows never participate.
fn group_bursts<F>(results: &mut Vec<SearchResult>, burst_for: F)
where
    F: Fn(&SearchResult) -> Option<u64>,
{
    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, result) in results.iter().enumerate() {
        if result.is_dir {
            continue;
        }
        let is_image = std::path::Path::new(&result.filename)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(crate::indexer::extract::image::is_supported_image_extension);
        if is_image && let Some(burst) = burst_for(result) {
            groups.entry(burst).or_default().push(i);
        }
    }

    let mut remove = HashSet::new();
    for indices in groups.values() {
        if indices.len() < 2 {
            continue;
        }
        let keep = indices[0];
        let others: Vec<String> = indices[1..]
            .iter()
            .map(|&i| results[i].path.clone())
            .collect();
        results[keep].group_count = indices.len();
        results[keep].group_paths = others;
        remove.extend(indices[1..].iter().copied());
    }
    let mut i = 0;
    results.retain(|_| {
        let keep = !remove.contains(&i);
        i += 1;
        keep
    });
}

/// Result ordering. `Best` preserves the fused relevance ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Best,
    Newest,
    Oldest,
    Largest,
    Name,
}

impl SortOrder {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_lowercase().as_str() {
            "newest" | "recent" => Self::Newest,
            "oldest" => Self::Oldest,
            "largest" | "biggest" | "size" => Self::Largest,
            "name" | "filename" => Self::Name,
            _ => Self::Best,
        }
    }
}

fn apply_sort(results: &mut [SearchResult], sort: SortOrder) {
    match sort {
        SortOrder::Best => results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
        }),
        SortOrder::Newest => results.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.path.cmp(&b.path))
        }),
        SortOrder::Oldest => results.sort_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.path.cmp(&b.path))
        }),
        SortOrder::Largest => results.sort_by(|a, b| {
            b.file_size
                .cmp(&a.file_size)
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.path.cmp(&b.path))
        }),
        // Cached key: a comparator allocating two Strings per comparison was
        // allocating O(n log n) times to order the page.
        SortOrder::Name => results.sort_by_cached_key(|result| {
            (result.filename.to_lowercase(), result.path.clone())
        }),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub seq: u64,
    pub active_filters: Vec<ActiveFilterDto>,
    pub hints: Vec<String>,
    pub search_time_ms: u64,
    /// Count of matches before the display limit (collected count for
    /// `mode:exhaustive`).
    pub total: usize,
    /// `"ranked"` or `"exhaustive"`.
    pub mode: String,
    /// Natural-language interpretations applied to this query.
    #[serde(default)]
    pub interpretation: Vec<crate::query::intent::InterpretationChip>,
    /// Server-side counts over the full matched set (before the display limit).
    #[serde(default)]
    pub facets: Facets,
}

pub struct SearchDispatcher;

impl SearchDispatcher {
    pub fn search(
        query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        semantic_enabled: bool,
        limit: usize,
    ) -> Vec<SearchResult> {
        let resp = Self::search_response(
            query,
            metadata,
            content_index,
            unit_store,
            semantic_enabled,
            limit,
            0,
        );
        resp.results
    }

    /// Runs a query and attaches snippets for the top results.
    ///
    /// Snippets come from the extraction-free fast path, so this stays cheap
    /// even for PDFs and images.
    pub fn search_response(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
    ) -> SearchResponse {
        Self::search_response_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            None,
            semantic_enabled,
            limit,
            seq,
            true,
            SortOrder::Best,
            &[],
            0,
            None,
        )
    }

    /// Runs a query with visual vector search enabled.
    #[allow(clippy::too_many_arguments)]
    pub fn search_response_with_visual(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        visual_store: Option<&crate::embed::visual::VisualStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
    ) -> SearchResponse {
        Self::search_response_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            visual_store,
            semantic_enabled,
            limit,
            seq,
            false,
            SortOrder::Best,
            &[],
            0,
            None,
        )
    }

    /// Runs a query with personal-vocabulary aliases applied.
    #[allow(clippy::too_many_arguments)]
    pub fn search_response_with_aliases(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
        aliases: &[(String, Vec<String>)],
    ) -> SearchResponse {
        Self::search_response_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            None,
            semantic_enabled,
            limit,
            seq,
            true,
            SortOrder::Best,
            aliases,
            0,
            None,
        )
    }

    /// Runs a query without touching file contents at all.
    ///
    /// Used by the Tauri `search` command: the UI renders results immediately
    /// and requests snippets separately through `get_snippets`.
    #[allow(clippy::too_many_arguments)]
    pub fn search_response_fast(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
    ) -> SearchResponse {
        Self::search_response_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            None,
            semantic_enabled,
            limit,
            seq,
            false,
            SortOrder::Best,
            &[],
            0,
            None,
        )
    }

    /// Sorted variant used by the desktop UI.
    #[allow(clippy::too_many_arguments)]
    pub fn search_response_fast_sorted(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
        sort: SortOrder,
        aliases: &[(String, Vec<String>)],
        offset: usize,
    ) -> SearchResponse {
        Self::search_response_fast_sorted_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            semantic_enabled,
            limit,
            seq,
            sort,
            aliases,
            offset,
            false,
        )
    }

    /// Sorted variant with an explicit snippet switch. The daemon uses this so
    /// CLI callers keep snippets while MCP can batch its own extraction.
    #[allow(clippy::too_many_arguments)]
    pub fn search_response_fast_sorted_with_snippets(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
        sort: SortOrder,
        aliases: &[(String, Vec<String>)],
        offset: usize,
        with_snippets: bool,
    ) -> SearchResponse {
        Self::search_response_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            None,
            semantic_enabled,
            limit,
            seq,
            with_snippets,
            sort,
            aliases,
            offset,
            None,
        )
    }

    /// Exhaustive keyset pagination: like
    /// [`Self::search_response_fast_sorted_with_snippets`] but only returning
    /// matches strictly after `start_after` (a path keyset cursor). Used by
    /// the agent surfaces to page past the 10k exhaustive cap without
    /// materialising more than one window at a time. Sort is forced to path
    /// order: keyset pagination is only meaningful over a stable order.
    #[allow(clippy::too_many_arguments)]
    pub fn search_response_exhaustive_after(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
        aliases: &[(String, Vec<String>)],
        start_after: &str,
        with_snippets: bool,
    ) -> SearchResponse {
        Self::search_response_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            None,
            semantic_enabled,
            limit,
            seq,
            with_snippets,
            SortOrder::Best,
            aliases,
            0,
            Some(start_after),
        )
    }

    /// Sorted variant with snippets and visual store support.
    #[allow(clippy::too_many_arguments)]
    pub fn search_response_fast_sorted_with_snippets_and_visual(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        visual_store: Option<&crate::embed::visual::VisualStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
        sort: SortOrder,
        aliases: &[(String, Vec<String>)],
        offset: usize,
        with_snippets: bool,
    ) -> SearchResponse {
        Self::search_response_with_snippets(
            raw_query,
            metadata,
            content_index,
            unit_store,
            visual_store,
            semantic_enabled,
            limit,
            seq,
            with_snippets,
            sort,
            aliases,
            offset,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn search_response_with_snippets(
        raw_query: &str,
        metadata: &MetadataStore,
        content_index: &Option<ContentIndex>,
        unit_store: Option<&UnitStore>,
        visual_store: Option<&crate::embed::visual::VisualStore>,
        semantic_enabled: bool,
        limit: usize,
        seq: u64,
        with_snippets: bool,
        sort: SortOrder,
        aliases: &[(String, Vec<String>)],
        offset: usize,
        start_after: Option<&str>,
    ) -> SearchResponse {
        let start_time = SystemTime::now();
        let trimmed = raw_query.trim();
        if trimmed.is_empty() {
            return SearchResponse {
                results: Vec::new(),
                seq,
                active_filters: Vec::new(),
                hints: Vec::new(),
                search_time_ms: 0,
                total: 0,
                mode: "ranked".to_string(),
                interpretation: Vec::new(),
                facets: Facets::default(),
            };
        }

        // Deterministic natural-language pass. Explicit grammar is left alone.
        let intent = crate::query::intent::compile_with_aliases(raw_query, aliases);
        let effective_query = intent.query.as_str();
        let sort = if sort == SortOrder::Best {
            intent
                .sort
                .as_deref()
                .map(SortOrder::parse)
                .unwrap_or(SortOrder::Best)
        } else {
            sort
        };

        let now_sec = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let trie = metadata.get_directory_trie();
        // Materializing a FileEntry reconstructs the path from the node arena,
        // joins a PathBuf and copies the name: several allocations per file,
        // for every query. On a 25k-file corpus that is ~100k allocations per
        // keystroke, which is the arena's whole purpose undone by the query
        // path. The store bumps `generation` on every mutation that can change
        // this list, so caching against it is a single comparison.
        // Cached inside the store, per generation: see `entries_snapshot`.
        let generation = metadata.generation();
        let entries = metadata.entries_snapshot();
        let needs_trie = query_needs_trie(effective_query);
        let path_index: HashMap<PathBuf, usize> = if needs_trie {
            entries
                .iter()
                .enumerate()
                .map(|(i, e)| (e.path.clone(), i))
                .collect()
        } else {
            HashMap::new()
        };
        // Prefer the store's path index. Some synthetic/partial stores (for
        // example a fixture that inserts files without their parent
        // directories) cannot resolve every path, so build a fallback map from
        // the materialized entries on first miss.
        let fallback_path_index: std::cell::RefCell<Option<HashMap<PathBuf, usize>>> =
            std::cell::RefCell::new(None);
        let resolve_index = |path: &Path| -> Option<usize> {
            if let Some(index) = path_index.get(path) {
                return Some(*index);
            }
            if let Some(index) = metadata.get_index(path) {
                return Some(index as usize);
            }
            let mut fallback = fallback_path_index.borrow_mut();
            if fallback.is_none() {
                *fallback = Some(
                    entries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| (entry.path.clone(), index))
                        .collect(),
                );
            }
            fallback.as_ref().and_then(|index| index.get(path).copied())
        };

        // 1. Parse AST from query
        let mut parsed = parse_query(
            effective_query,
            &entries,
            &path_index,
            needs_trie.then_some(&trie),
            generation,
        );
        let route = crate::query::router::route_query(&parsed);

        // Pre-compile filters once per query (lowercase strings, intern extensions)
        let compiled_filters: Vec<CompiledFilter> =
            parsed.filters.iter().map(CompiledFilter::from).collect();
        // Positive ext/size clauses can safely narrow the Tantivy query; the
        // normal metadata filter still runs afterwards.
        let pushdown_filters = content_pushdown(&parsed.filters);

        // 2. Resolve scopes and filter candidates
        let candidate_indices: Vec<usize> = if !parsed.scopes.is_empty() {
            let mut included = HashSet::new();
            let mut has_include_scope = false;

            for scope in &parsed.scopes {
                if scope.include {
                    has_include_scope = true;
                    let scope_candidates =
                        trie.get_scope_candidates(scope.dir_id as usize, generation);
                    for &idx in scope_candidates.iter() {
                        included.insert(idx);
                    }
                }
            }

            let base_set: Vec<usize> = if has_include_scope {
                included.into_iter().collect()
            } else {
                (0..entries.len()).collect()
            };

            // Remove any excluded scopes
            let mut excluded = HashSet::new();
            for scope in &parsed.scopes {
                if !scope.include {
                    let scope_candidates =
                        trie.get_scope_candidates(scope.dir_id as usize, generation);
                    for &idx in scope_candidates.iter() {
                        excluded.insert(idx);
                    }
                }
            }

            base_set
                .into_iter()
                .filter(|idx| !excluded.contains(idx))
                .filter(|&idx| {
                    if let Some(entry) = entries.get(idx) {
                        matches_all_compiled_filters(&compiled_filters, entry)
                    } else {
                        false
                    }
                })
                .collect()
        } else if !parsed.filters.is_empty() {
            (0..entries.len())
                .filter(|&idx| matches_all_compiled_filters(&compiled_filters, &entries[idx]))
                .collect()
        } else {
            (0..entries.len()).collect()
        };

        // Extract positive terms text, split by field scoping (name:/content:/path:)
        // so that field-scoped terms only search their target field. Regex terms
        // (`re:`) never enter the fuzzy/BM25 text tiers.
        let mut fuzzy_terms: Vec<&str> = Vec::new(); // Any, Name, Path → fuzzy (filename + path)
        let mut bm25_groups: Vec<(FieldKind, &str)> = Vec::new();
        let mut regex_patterns: Vec<&str> = Vec::new();
        let has_near = !parsed.proximity.is_empty();

        for t in &parsed.terms {
            if t.is_regex {
                regex_patterns.push(&t.text);
            }
        }

        let mut proximity_phrase_strings: Vec<String> = Vec::new();

        if has_near {
            let mut involved: HashSet<usize> = HashSet::new();
            for (i, j, _) in &parsed.proximity {
                involved.insert(*i);
                involved.insert(*j);
            }
            for (i, j, distance) in &parsed.proximity {
                // Pass the canonical `near[x]` form through to the content
                // index, which knows how to rewrite it into a phrase query.
                // Terms are sanitized by that rewrite.
                let expr = format!(
                    "{} near[{}] {}",
                    parsed.terms[*i].text, distance, parsed.terms[*j].text
                );
                proximity_phrase_strings.push(expr);
            }
            for expr in &proximity_phrase_strings {
                bm25_groups.push((FieldKind::Any, expr.as_str()));
            }
            for (idx, t) in parsed.terms.iter().enumerate() {
                if t.is_regex || involved.contains(&idx) {
                    continue;
                }
                match t.field {
                    TermField::Any => {
                        bm25_groups.push((FieldKind::Any, &t.text));
                    }
                    TermField::Name => {
                        bm25_groups.push((FieldKind::Name, &t.text));
                    }
                    TermField::Content => {
                        bm25_groups.push((FieldKind::Content, &t.text));
                    }
                    TermField::Path => {}
                }
            }
        } else {
            for t in &parsed.terms {
                if t.is_regex {
                    continue;
                }
                match t.field {
                    TermField::Any => {
                        fuzzy_terms.push(&t.text);
                        bm25_groups.push((FieldKind::Any, &t.text));
                    }
                    TermField::Name => {
                        fuzzy_terms.push(&t.text);
                        bm25_groups.push((FieldKind::Name, &t.text));
                    }
                    TermField::Content => {
                        bm25_groups.push((FieldKind::Content, &t.text));
                    }
                    TermField::Path => {
                        fuzzy_terms.push(&t.text);
                    }
                }
            }
        }
        let fuzzy_text = fuzzy_terms.join(" ");
        let bm25_text: String = bm25_groups
            .iter()
            .map(|(_, text)| *text)
            .collect::<Vec<_>>()
            .join(" ");
        let search_text = if fuzzy_text.is_empty() {
            bm25_text.as_str()
        } else {
            fuzzy_text.as_str()
        };
        let search_text_lower = search_text.to_lowercase();

        // Membership checks are free when the candidate list is the whole
        // corpus (the common unfiltered query).
        let candidate_set = CandidateSet::new(&candidate_indices, entries.len());

        // The node-id map only serves the semantic/visual tiers. Building a
        // million-entry map for a plain filename query is pure overhead.
        let wants_semantic = semantic_enabled
            && unit_store.is_some()
            && !search_text.is_empty()
            && !has_near
            && ModelRegistry::global().is_enabled();
        // No encoder means the visual tier never runs; building the id map for it
        // would cost O(corpus) on every text query for nothing.
        let wants_visual = crate::embed::visual::visual_available()
            && visual_store.is_some()
            && !search_text.is_empty()
            && !has_near;
        let (node_to_idx, allowed_node_ids): (HashMap<u64, usize>, Option<HashSet<u64>>) =
            if wants_semantic || wants_visual {
                let mut map = HashMap::new();
                for &idx in &candidate_indices {
                    if let Some(entry) = entries.get(idx)
                        && entry.id > 0
                    {
                        map.insert(entry.id, idx);
                    }
                }
                // An unfiltered query allows every node, so the set filters nothing.
                let allowed = if map.is_empty() || candidate_set.all {
                    None
                } else {
                    Some(map.keys().copied().collect())
                };
                (map, allowed)
            } else {
                (HashMap::new(), None)
            };

        // 2b. Exhaustive mode: no fuzzy/semantic tiers, enumerate by path.
        if parsed.mode == Mode::Exhaustive {
            let mut hints = parsed.hints;
            let mut results = exhaustive_results(
                content_index,
                &entries,
                &path_index,
                metadata,
                &candidate_set,
                &candidate_indices,
                &bm25_groups,
                &regex_patterns,
                EXHAUSTIVE_LIMIT,
                &mut hints,
                start_after,
            );
            // Keyset pages must stay path-ordered for the cursor to work;
            // user sorts only apply to offset-based pages.
            if start_after.is_none() {
                apply_sort(&mut results, sort);
            }
            let total = results.len();
            let facets = compute_facets(&results, now_sec);
            if results.len() > EXHAUSTIVE_LIMIT {
                results.truncate(EXHAUSTIVE_LIMIT);
            }
            if results.is_empty() && !parsed.active_filters_dto.is_empty() {
                hints.push("No results match the active filters; try removing one.".to_string());
            }
            let elapsed = start_time
                .elapsed()
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            return SearchResponse {
                results,
                seq,
                active_filters: parsed.active_filters_dto,
                hints,
                search_time_ms: elapsed,
                total,
                mode: "exhaustive".to_string(),
                interpretation: intent.chips.clone(),
                facets,
            };
        }

        // 3. Tier 1: Fuzzy filename search (< 5ms)
        let mut fuzzy_ranks: HashMap<usize, usize> = HashMap::new();
        if !fuzzy_text.is_empty() {
            let mut fuzzy_searcher = FuzzySearcher::new();
            // Search across candidate entries by index (no clone needed)
            let fuzzy_matches =
                fuzzy_searcher.search(&fuzzy_text, &entries, &candidate_indices, limit * 3);
            for (rank, (orig_idx, _score)) in fuzzy_matches.into_iter().enumerate() {
                fuzzy_ranks.insert(orig_idx, rank + 1);
            }
        }

        // 4. Tier 2: Tantivy BM25 content search (< 50ms)
        // A single include scope is pushed into the Tantivy query as a path
        // prefix, so BM25 never scores documents outside the scope.
        let scope_prefix: Option<String> = {
            let mut includes = parsed.scopes.iter().filter(|s| s.include);
            match (includes.next(), includes.next()) {
                (Some(scope), None) => Some(scope.path.to_string_lossy().to_string()),
                _ => None,
            }
        };
        let mut bm25_ranks: HashMap<PathBuf, usize> = HashMap::new();
        if (!bm25_groups.is_empty() || !regex_patterns.is_empty())
            && let Some(c_index) = content_index
        {
            let outcome = c_index.search_groups_filtered(
                &bm25_groups,
                &regex_patterns,
                limit * 3,
                scope_prefix.as_deref(),
                &pushdown_filters,
            );
            parsed.hints.extend(outcome.hints);
            for (rank, cm) in outcome.matches.into_iter().enumerate() {
                bm25_ranks.insert(cm.path, rank + 1);
            }
        }

        // 5. Tier 3: Static semantic vector scan (< 5ms)
        // Weak vectors are kept in the map (with their raw cosine) so hits that
        // also match fuzzy/BM25 still contribute; the floor below is applied
        // only when creating a semantic-only candidate.
        let model = ModelRegistry::global();
        let semantic_floor = if model.has_static_table() { 0.28 } else { 0.12 };
        let mut semantic_ranks: HashMap<PathBuf, (usize, f32, (usize, usize))> = HashMap::new();
        if semantic_enabled
            && model.is_enabled()
            && !search_text.is_empty()
            && !has_near
            && let Some(store) = unit_store
            && !store.is_empty()
        {
            let query_vec = model.embed_to_i8(search_text);
            let hits = ExactScanner::scan_with_model(
                &query_vec,
                store,
                allowed_node_ids.as_ref(),
                limit * 3,
                Some(model.model_id),
            );
            for (rank, hit) in hits.into_iter().enumerate() {
                let orig_idx = match node_to_idx.get(&hit.node_id).copied() {
                    Some(idx) => idx,
                    // Fallback for stores whose entries carry no node ids.
                    None if node_to_idx.is_empty() => metadata
                        .get_by_node_id(hit.node_id)
                        .and_then(|entry| resolve_index(&entry.path))
                        .unwrap_or(usize::MAX),
                    None => continue,
                };
                if orig_idx != usize::MAX
                    && candidate_set.contains(orig_idx)
                    && let Some(entry) = entries.get(orig_idx)
                {
                    semantic_ranks.insert(
                        entry.path.clone(),
                        (
                            rank + 1,
                            hit.score,
                            (hit.best_unit.byte_start, hit.best_unit.byte_end),
                        ),
                    );
                }
            }
        }

        // 5b. Tier 4: Visual vector scan over image nodes (< 5ms)
        let visual_floor = 0.20f32;
        let mut visual_ranks: HashMap<PathBuf, (usize, f32)> = HashMap::new();
        if !search_text.is_empty()
            && !has_near
            && let Some(v_store) = visual_store
            && !v_store.is_empty()
            && let Some(encoder) = crate::embed::visual::active_encoder()
        {
            let query_vec = encoder.embed_text(search_text);
            let hits = v_store.scan(
                &query_vec,
                allowed_node_ids.as_ref(),
                visual_floor,
                limit * 3,
            );
            for (rank, (node_id, score)) in hits.into_iter().enumerate() {
                let orig_idx = match node_to_idx.get(&node_id).copied() {
                    Some(idx) => idx,
                    None if node_to_idx.is_empty() => metadata
                        .get_by_node_id(node_id)
                        .and_then(|entry| resolve_index(&entry.path))
                        .unwrap_or(usize::MAX),
                    None => continue,
                };
                if orig_idx != usize::MAX
                    && candidate_set.contains(orig_idx)
                    && let Some(entry) = entries.get(orig_idx)
                {
                    let is_image = entry
                        .path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(crate::indexer::extract::image::is_supported_image_extension);
                    if is_image {
                        visual_ranks.insert(entry.path.clone(), (rank + 1, score));
                    }
                }
            }
        }

        // 6. 4-tier RRF Fusion (k = 60.0) with dynamic route weights
        struct MatchCandidate {
            entry_idx: usize,
            in_fuzzy: bool,
            in_bm25: bool,
            in_semantic: bool,
            in_visual: bool,
            in_folded: bool,
            rrf_score: f64,
            best_range: Option<(usize, usize)>,
        }

        let mut candidates: HashMap<PathBuf, MatchCandidate> = HashMap::new();

        if fuzzy_text.is_empty() && bm25_groups.is_empty() && regex_patterns.is_empty() {
            // Filter-only / folder navigation browsing mode
            for &idx in &candidate_indices {
                if let Some(entry) = entries.get(idx) {
                    candidates.insert(
                        entry.path.clone(),
                        MatchCandidate {
                            entry_idx: idx,
                            in_fuzzy: true,
                            in_bm25: false,
                            in_semantic: false,
                            in_visual: false,
                            in_folded: false,
                            rrf_score: 0.1,
                            best_range: None,
                        },
                    );
                }
            }
        } else {
            let (w_fuzzy, w_bm25, w_semantic, w_visual) = match route {
                crate::query::router::QueryRoute::NameCode => (1.3, 0.8, 0.8, 0.5),
                crate::query::router::QueryRoute::ContentPhrase => (0.8, 1.3, 1.0, 0.6),
                crate::query::router::QueryRoute::SemanticSearch => (0.8, 1.0, 1.5, 1.2),
                crate::query::router::QueryRoute::VisualSearch => (0.8, 0.6, 1.0, 2.0),
                _ => (1.0, 1.0, 1.0, 0.8),
            };

            // Merge fuzzy matches
            for (&idx, &rank) in &fuzzy_ranks {
                if let Some(entry) = entries.get(idx) {
                    let rrf = w_fuzzy * (1.0 / (60.0 + (rank as f64)));
                    candidates.insert(
                        entry.path.clone(),
                        MatchCandidate {
                            entry_idx: idx,
                            in_fuzzy: true,
                            in_bm25: false,
                            in_semantic: false,
                            in_visual: false,
                            in_folded: false,
                            rrf_score: rrf,
                            best_range: None,
                        },
                    );
                }
            }

            // Merge BM25 matches
            for (path, &rank) in &bm25_ranks {
                if let Some(orig_idx) = resolve_index(path) {
                    if !candidate_set.contains(orig_idx) {
                        continue;
                    }
                    let rrf = w_bm25 * (1.0 / (60.0 + (rank as f64)));
                    if let Some(cand) = candidates.get_mut(path) {
                        cand.in_bm25 = true;
                        cand.rrf_score += rrf;
                    } else {
                        candidates.insert(
                            path.clone(),
                            MatchCandidate {
                                entry_idx: orig_idx,
                                in_fuzzy: false,
                                in_bm25: true,
                                in_semantic: false,
                                in_visual: false,
                                in_folded: false,
                                rrf_score: rrf,
                                best_range: None,
                            },
                        );
                    }
                }
            }

            // Merge Semantic matches
            for (path, &(rank, cosine, range)) in &semantic_ranks {
                let Some(orig_idx) = resolve_index(path) else {
                    continue;
                };
                if !candidate_set.contains(orig_idx) {
                    continue;
                }
                let rrf = w_semantic * (1.0 / (60.0 + (rank as f64)));
                if let Some(cand) = candidates.get_mut(path) {
                    cand.in_semantic = true;
                    cand.rrf_score += rrf;
                    cand.best_range = Some(range);
                } else {
                    // Semantic-only candidates must clear the relevance floor;
                    // hits already found by fuzzy/BM25 are merged above.
                    if cosine < semantic_floor {
                        continue;
                    }
                    candidates.insert(
                        path.clone(),
                        MatchCandidate {
                            entry_idx: orig_idx,
                            in_fuzzy: false,
                            in_bm25: false,
                            in_semantic: true,
                            in_visual: false,
                            in_folded: false,
                            rrf_score: rrf,
                            best_range: Some(range),
                        },
                    );
                }
            }

            // Merge Visual matches
            for (path, &(rank, cosine)) in &visual_ranks {
                let Some(orig_idx) = resolve_index(path) else {
                    continue;
                };
                if !candidate_set.contains(orig_idx) {
                    continue;
                }
                let rrf = w_visual * (1.0 / (60.0 + (rank as f64)));
                if let Some(cand) = candidates.get_mut(path) {
                    cand.in_visual = true;
                    cand.rrf_score += rrf;
                } else {
                    if cosine < visual_floor {
                        continue;
                    }
                    candidates.insert(
                        path.clone(),
                        MatchCandidate {
                            entry_idx: orig_idx,
                            in_fuzzy: false,
                            in_bm25: false,
                            in_semantic: false,
                            in_visual: true,
                            in_folded: false,
                            rrf_score: rrf,
                            best_range: None,
                        },
                    );
                }
            }

            // Tier 4: folded trigrams, only when the exact/fuzzy/semantic
            // tiers left a shortfall. The lower weight keeps these after
            // exact hits, and raw-text verification has already rejected
            // broad n-gram noise.
            if candidates.len() < limit
                && !search_text.is_empty()
                && !has_near
                && let Some(c_index) = content_index
            {
                let w_folded = w_bm25 * 0.55;
                for (rank, cm) in c_index
                    .search_folded_filtered(
                        search_text,
                        limit * 3,
                        scope_prefix.as_deref(),
                        &pushdown_filters,
                    )
                    .into_iter()
                    .enumerate()
                {
                    let Some(orig_idx) = resolve_index(&cm.path) else {
                        continue;
                    };
                    if !candidate_set.contains(orig_idx) {
                        continue;
                    }
                    let rrf = w_folded * (1.0 / (60.0 + rank as f64));
                    if let Some(cand) = candidates.get_mut(&cm.path) {
                        cand.in_folded = true;
                        cand.rrf_score += rrf;
                    } else {
                        candidates.insert(
                            cm.path,
                            MatchCandidate {
                                entry_idx: orig_idx,
                                in_fuzzy: false,
                                in_bm25: false,
                                in_semantic: false,
                                in_visual: false,
                                in_folded: true,
                                rrf_score: rrf,
                                best_range: None,
                            },
                        );
                    }
                }
            }
        }

        // 7. Apply ranking bonuses & build SearchResults
        // Normalize RRF before bonuses so an absolute depth term cannot
        // dominate the fused relevance signal.
        let max_rrf = candidates
            .values()
            .map(|candidate| candidate.rrf_score)
            .fold(0.0f64, f64::max);

        let mut results: Vec<SearchResult> = candidates
            .into_iter()
            .filter_map(|(path, cand)| {
                let entry = entries.get(cand.entry_idx)?;

                // MatchType resolution: Semantic badge if only found by semantic tier,
                // Visual badge if only found by visual tier
                let match_type = if cand.in_visual
                    && !cand.in_fuzzy
                    && !cand.in_bm25
                    && !cand.in_semantic
                    && !cand.in_folded
                {
                    MatchType::Visual
                } else if cand.in_semantic
                    && !cand.in_fuzzy
                    && !cand.in_bm25
                    && !cand.in_visual
                    && !cand.in_folded
                {
                    MatchType::Semantic
                } else if cand.in_fuzzy
                    && (cand.in_bm25 || cand.in_semantic || cand.in_visual || cand.in_folded)
                {
                    MatchType::Both
                } else if cand.in_bm25 {
                    MatchType::Content
                } else if cand.in_folded {
                    MatchType::ContentFolded
                } else {
                    MatchType::FileName
                };

                let usage = metadata.get_usage(&entry.path);
                let score = finalize_score(
                    cand.rrf_score,
                    max_rrf,
                    entry,
                    &path,
                    &search_text_lower,
                    now_sec,
                    usage,
                );

                Some(SearchResult {
                    path: path.to_string_lossy().to_string(),
                    filename: entry.name.clone(),
                    snippet: None,
                    score,
                    file_size: entry.size,
                    modified: entry.modified,
                    is_dir: entry.is_dir,
                    match_type,
                    best_range: cand.best_range,
                    group_count: 1,
                    group_paths: Vec::new(),
                })
            })
            .collect();

        // 8. Ordering: best-match by default; explicit sort when requested.
        apply_sort(&mut results, sort);
        group_bursts(&mut results, |result| {
            metadata.burst_for_path(std::path::Path::new(&result.path))
        });
        group_versions(&mut results);

        let total = results.len();
        let facets = compute_facets(&results, now_sec);

        if offset > 0 {
            let skip = offset.min(results.len());
            results.drain(0..skip);
        }
        if results.len() > limit {
            results.truncate(limit);
        }

        // 9. Post-rank snippet extraction for top 20 files.
        //
        // `get_snippet_fast` reads plain text directly and otherwise only
        // consults the extraction cache, so the query path never runs a PDF,
        // Office or OCR extractor (the previous `read_text_content` fallback
        // did, which is what made queries take seconds on document-heavy
        // indexes).
        if with_snippets && !search_text.is_empty() {
            for item in results.iter_mut().take(20) {
                if item.is_dir {
                    continue;
                }
                if let Some(ci) = content_index.as_ref() {
                    item.snippet = ci.get_snippet_fast(Path::new(&item.path), search_text);
                }
            }
        }

        if results.is_empty() && !parsed.active_filters_dto.is_empty() {
            parsed
                .hints
                .push("No results match the active filters; try removing one.".to_string());
        }

        let elapsed = start_time
            .elapsed()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        SearchResponse {
            results,
            seq,
            active_filters: parsed.active_filters_dto,
            hints: parsed.hints,
            search_time_ms: elapsed,
            total,
            mode: "ranked".to_string(),
            interpretation: intent.chips,
            facets,
        }
    }
}

/// Final score for a fused candidate.
///
/// RRF is normalized to `[0, 1]` before bonuses so scores are comparable
/// regardless of how many tiers matched. The depth penalty is bounded (max
/// `-0.10`) and can never push a score below zero.
fn finalize_score(
    rrf_score: f64,
    max_rrf: f64,
    entry: &FileEntry,
    path: &Path,
    search_text_lower: &str,
    now_sec: u64,
    usage: (u32, u64),
) -> f64 {
    let base = if max_rrf > 0.0 {
        rrf_score / max_rrf
    } else {
        0.0
    };
    let mut score = base;

    // Exact substring in name: +0.20
    let name_lower = entry.name.to_lowercase();
    if !search_text_lower.is_empty() && name_lower.contains(search_text_lower) {
        score += 0.20;
    }

    // Prefix in name: +0.15
    if !search_text_lower.is_empty() && name_lower.starts_with(search_text_lower) {
        score += 0.15;
    }

    // Smooth recency decay: +0.10 * exp(-age_days / 7)
    let age_days = (now_sec.saturating_sub(entry.modified) as f64) / 86400.0;
    score += 0.10 * (-age_days / 7.0).exp();

    // Frecency bonus: +0.08 * log1p(opens) * exp(-last_open_days / 14)
    let (opens, last_open) = usage;
    if opens > 0 {
        let last_open_days = (now_sec.saturating_sub(last_open) as f64) / 86400.0;
        score += 0.08 * (opens as f64).ln_1p() * (-last_open_days / 14.0).exp();
    }

    // Bounded depth penalty: at most -0.10, saturating at depth 20.
    let depth = path.components().count();
    score -= 0.10 * ((depth as f64) / 20.0).min(1.0);

    score.max(0.0)
}

/// Candidate membership for fusion. When the candidate list already covers
/// every entry, skip the million-key hash set entirely.
struct CandidateSet {
    all: bool,
    indices: HashSet<usize>,
}

impl CandidateSet {
    fn new(candidate_indices: &[usize], total: usize) -> Self {
        let all = candidate_indices.len() == total;
        Self {
            all,
            indices: if all {
                HashSet::new()
            } else {
                candidate_indices.iter().copied().collect()
            },
        }
    }

    fn contains(&self, index: usize) -> bool {
        self.all || self.indices.contains(&index)
    }
}

/// True when the query can use the directory trie (`in:` or a leading
/// directory-like token). Other queries skip the path-index build entirely.
fn query_needs_trie(query: &str) -> bool {
    let lowered = query.to_ascii_lowercase();
    if lowered.contains("in:") {
        return true;
    }
    let first = query.split_whitespace().next().unwrap_or("");
    first.ends_with('/')
        || first.ends_with('\\')
        || first.starts_with("~/")
        || first.starts_with("./")
}

/// Enumerates matches for `mode:exhaustive`, ordered by path.
///
/// Fuzzy and semantic tiers are deliberately skipped: they rank, they do not
/// enumerate. The returned vector is capped only by the caller.
#[allow(clippy::too_many_arguments)]
fn exhaustive_results(
    content_index: &Option<ContentIndex>,
    entries: &[FileEntry],
    path_index: &HashMap<PathBuf, usize>,
    metadata: &MetadataStore,
    candidate_set: &CandidateSet,
    candidate_indices: &[usize],
    groups: &[(FieldKind, &str)],
    regex_patterns: &[&str],
    cap: usize,
    hints: &mut Vec<String>,
    start_after: Option<&str>,
) -> Vec<SearchResult> {
    let mut results = Vec::new();

    let after_keyset = |path: &std::path::Path| -> bool {
        match start_after {
            Some(start) => path.as_os_str() > std::ffi::OsStr::new(start),
            None => true,
        }
    };

    if groups.is_empty() && regex_patterns.is_empty() {
        for &idx in candidate_indices {
            if let Some(entry) = entries.get(idx) {
                if !after_keyset(&entry.path) {
                    continue;
                }
                results.push(SearchResult {
                    path: entry.path.to_string_lossy().to_string(),
                    filename: entry.name.clone(),
                    snippet: None,
                    score: 0.0,
                    file_size: entry.size,
                    modified: entry.modified,
                    is_dir: entry.is_dir,
                    match_type: MatchType::FileName,
                    best_range: None,
                    group_count: 1,
                    group_paths: Vec::new(),
                });
            }
        }
    } else if let Some(c_index) = content_index {
        let outcome = c_index.search_groups_with_regex(groups, regex_patterns, cap, None);
        hints.extend(outcome.hints);
        for cm in outcome.matches {
            if !after_keyset(&cm.path) {
                continue;
            }
            let Some(idx) = path_index
                .get(&cm.path)
                .copied()
                .or_else(|| metadata.get_index(&cm.path).map(|idx| idx as usize))
            else {
                continue;
            };
            if !candidate_set.contains(idx) {
                continue;
            }
            let Some(entry) = entries.get(idx) else {
                continue;
            };
            results.push(SearchResult {
                path: cm.path.to_string_lossy().to_string(),
                filename: entry.name.clone(),
                snippet: None,
                score: cm.score as f64,
                file_size: entry.size,
                modified: entry.modified,
                is_dir: entry.is_dir,
                match_type: MatchType::Content,
                best_range: None,
                group_count: 1,
                group_paths: Vec::new(),
            });
        }
    }

    results.sort_by(|a, b| a.path.cmp(&b.path));

    if results.len() >= cap {
        hints.push(format!(
            "Exhaustive mode capped at {} results; refine the query to narrow it.",
            cap
        ));
    }

    results
}

/// Positive ext/size filters that can be pushed into the content index.
///
/// Only the first positive clause of each kind is pushed: it is a necessary
/// condition for every result, so the pushdown can never drop a true match.
fn content_pushdown(filters: &[Filter]) -> ContentFilters {
    let mut pushdown = ContentFilters::default();
    for filter in filters {
        match filter {
            Filter::Ext(ext) if pushdown.ext.is_none() => {
                pushdown.ext = Some(ext.to_lowercase());
            }
            Filter::Size(range) if pushdown.size.is_none() => {
                pushdown.size = Some(*range);
            }
            _ => {}
        }
    }
    pushdown
}

fn matches_all_compiled_filters(filters: &[CompiledFilter], entry: &FileEntry) -> bool {
    for f in filters {
        if !f.matches(entry) {
            return false;
        }
    }
    true
}

/// Pre-compiled filter that pre-lowercases strings to avoid per-entry allocation.
#[derive(Clone)]
enum CompiledFilter {
    Ext(String),
    Type(String),
    Modified(crate::query::ast::RangeOp),
    Size(crate::query::ast::RangeOp),
    Kind(crate::query::ast::FileKind),
    Lang(String),
    Name(String),
    Path(String),
    Not(Box<CompiledFilter>),
}

impl CompiledFilter {
    fn from(f: &Filter) -> Self {
        match f {
            Filter::Ext(e) => CompiledFilter::Ext(e.to_lowercase()),
            Filter::Type(t) => CompiledFilter::Type(t.to_lowercase()),
            Filter::Modified(r) => CompiledFilter::Modified(*r),
            Filter::Size(r) => CompiledFilter::Size(*r),
            Filter::Kind(k) => CompiledFilter::Kind(*k),
            Filter::Lang(l) => CompiledFilter::Lang(l.to_lowercase()),
            Filter::Name(n) => CompiledFilter::Name(n.to_lowercase()),
            Filter::Path(p) => CompiledFilter::Path(p.to_lowercase()),
            Filter::Not(inner) => CompiledFilter::Not(Box::new(CompiledFilter::from(inner))),
        }
    }

    fn matches(&self, entry: &FileEntry) -> bool {
        match self {
            CompiledFilter::Ext(ext) => entry
                .extension
                .map(|e| e.eq_ignore_ascii_case(ext))
                .unwrap_or(false),
            CompiledFilter::Type(typ) => matches_type_category(typ, entry.extension, entry.is_dir),
            CompiledFilter::Modified(range) => range.matches(entry.modified),
            CompiledFilter::Size(range) => range.matches(entry.size),
            CompiledFilter::Kind(crate::query::ast::FileKind::Dir) => entry.is_dir,
            CompiledFilter::Kind(crate::query::ast::FileKind::File) => !entry.is_dir,
            CompiledFilter::Lang(lang) => matches_language(lang, entry.extension),
            // Both needles are already lowercased when compiled, and these
            // run once per file in the corpus, so match without allocating.
            CompiledFilter::Name(name) => contains_ignore_case(&entry.name, name),
            CompiledFilter::Path(path_substr) => {
                contains_ignore_case(&entry.path.to_string_lossy(), path_substr)
            }
            CompiledFilter::Not(inner) => !inner.matches(entry),
        }
    }
}

// Removed: replaced by CompiledFilter::from + CompiledFilter::matches

// Removed: replaced by matches_all_compiled_filters + CompiledFilter

/// Case-insensitive substring test with no allocation on the ASCII path.
///
/// These run once per file in the corpus on every filtered query, so a
/// `to_lowercase()` per call was allocating once per file for a substring
/// check. Non-ASCII pairs still get the exact Unicode behaviour by falling
/// back to a lowercased comparison.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let (hay, needle_bytes) = (haystack.as_bytes(), needle.as_bytes());
    if hay.len() < needle_bytes.len() {
        return false;
    }
    if hay.is_ascii() && needle.is_ascii() {
        return hay
            .windows(needle_bytes.len())
            .any(|window| window.eq_ignore_ascii_case(needle_bytes));
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn ext_is(ext: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| ext.eq_ignore_ascii_case(candidate))
}

fn matches_type_category(typ: &str, ext_opt: Option<&str>, is_dir: bool) -> bool {
    let ext = match ext_opt {
        Some(e) => e,
        None => {
            if is_dir && ext_is(typ, &["dir", "directory", "folder"]) {
                return true;
            }
            return false;
        }
    };
    if ext_is(typ, &["image"]) {
        return ext_is(
            ext,
            &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "tiff", "tif"],
        );
    }
    if ext_is(typ, &["pdf"]) {
        return ext.eq_ignore_ascii_case("pdf");
    }
    if ext_is(typ, &["doc", "document"]) {
        return ext_is(
            ext,
            &["doc", "docx", "odt", "rtf", "pdf", "txt", "md", "pages", "epub"],
        );
    }
    if ext_is(typ, &["sheet", "spreadsheet", "table"]) {
        return ext_is(ext, &["xlsx", "xls", "ods", "csv", "tsv", "parquet", "numbers"]);
    }
    if ext_is(typ, &["presentation", "slide", "slides"]) {
        return ext_is(ext, &["pptx", "ppt", "odp", "key"]);
    }
    if ext_is(typ, &["ebook", "book"]) {
        return ext_is(ext, &["epub", "mobi", "azw3", "pdf"]);
    }
    if ext_is(typ, &["code"]) {
        return ext_is(
            ext,
            &[
                "rs", "ts", "tsx", "js", "jsx", "py", "go", "c", "cpp", "h", "hpp", "java", "kt",
                "swift", "rb", "php", "sh", "html", "css", "json", "toml", "yaml", "yml", "xml",
                "sql",
            ],
        );
    }
    if ext_is(typ, &["video"]) {
        return ext_is(ext, &["mp4", "mkv", "mov", "avi", "webm", "flv"]);
    }
    if ext_is(typ, &["audio"]) {
        return ext_is(ext, &["mp3", "wav", "flac", "aac", "ogg", "m4a"]);
    }
    if ext_is(typ, &["archive"]) {
        return ext_is(ext, &["zip", "tar", "gz", "bz2", "7z", "rar", "xz"]);
    }
    if ext_is(typ, &["dir", "directory", "folder"]) {
        return is_dir;
    }
    ext.eq_ignore_ascii_case(typ)
}

#[allow(dead_code)]
fn matches_language(lang: &str, ext_opt: Option<&str>) -> bool {
    let ext = match ext_opt {
        Some(e) => e,
        None => return false,
    };
    if ext_is(lang, &["rust"]) {
        return ext.eq_ignore_ascii_case("rs");
    }
    if ext_is(lang, &["python", "py"]) {
        return ext.eq_ignore_ascii_case("py");
    }
    if ext_is(lang, &["typescript", "ts"]) {
        return ext_is(ext, &["ts", "tsx"]);
    }
    if ext_is(lang, &["javascript", "js"]) {
        return ext_is(ext, &["js", "jsx", "mjs", "cjs"]);
    }
    if ext_is(lang, &["go"]) {
        return ext.eq_ignore_ascii_case("go");
    }
    if ext_is(lang, &["c"]) {
        return ext_is(ext, &["c", "h"]);
    }
    if ext_is(lang, &["cpp", "c++"]) {
        return ext_is(ext, &["cpp", "cc", "cxx", "hpp", "h"]);
    }
    if ext_is(lang, &["java"]) {
        return ext.eq_ignore_ascii_case("java");
    }
    if ext_is(lang, &["kotlin", "kt"]) {
        return ext_is(ext, &["kt", "kts"]);
    }
    if ext_is(lang, &["swift"]) {
        return ext.eq_ignore_ascii_case("swift");
    }
    if ext_is(lang, &["ruby", "rb"]) {
        return ext.eq_ignore_ascii_case("rb");
    }
    if ext_is(lang, &["php"]) {
        return ext.eq_ignore_ascii_case("php");
    }
    if ext_is(lang, &["shell", "bash", "sh"]) {
        return ext_is(ext, &["sh", "bash", "zsh"]);
    }
    if ext_is(lang, &["html"]) {
        return ext_is(ext, &["html", "htm"]);
    }
    if ext_is(lang, &["css"]) {
        return ext_is(ext, &["css", "scss", "sass"]);
    }
    if ext_is(lang, &["json"]) {
        return ext.eq_ignore_ascii_case("json");
    }
    if ext_is(lang, &["markdown", "md"]) {
        return ext_is(ext, &["md", "markdown"]);
    }
    if ext_is(lang, &["sql"]) {
        return ext.eq_ignore_ascii_case("sql");
    }
    ext.eq_ignore_ascii_case(lang)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::RangeOp;

    fn fake_result(path: &str) -> SearchResult {
        SearchResult {
            path: path.to_string(),
            filename: Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            snippet: None,
            score: 1.0,
            file_size: 1,
            modified: 1,
            is_dir: false,
            match_type: MatchType::FileName,
            best_range: None,
            group_count: 1,
            group_paths: Vec::new(),
        }
    }

    #[test]
    fn cached_entries_follow_the_store_generation() {
        use crate::indexer::walker::FileEntry;

        let entry = |path: &str| FileEntry {
            id: 0,
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            extension: None,
            size: 1,
            modified: 1,
            is_dir: false,
            dev: 0,
            ino: 0,
            char_bloom: 0,
            content_pending: false,
        };

        let mut store = MetadataStore::without_db();
        store.add(entry("/one.txt"));

        let first = store.entries_snapshot();
        assert_eq!(first.len(), 1);

        // Same generation: the cached list is reused, not rebuilt.
        let again = store.entries_snapshot();
        assert!(std::sync::Arc::ptr_eq(&first, &again));

        // A mutation bumps the generation, so the next query must see it.
        store.add(entry("/two.txt"));
        let after = store.entries_snapshot();
        assert_eq!(after.len(), 2, "a new entry must appear after a mutation");
        assert!(!std::sync::Arc::ptr_eq(&first, &after));
    }

    /// Run with `cargo test --release --lib burst_grouping_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn burst_grouping_cost_benchmark() {
        let base: Vec<SearchResult> = (0..50)
            .map(|i| fake_result(&format!("/shots/burst-{i:02}.png")))
            .collect();
        let bursts: HashMap<String, u64> = base
            .iter()
            .enumerate()
            .map(|(i, result)| (result.path.clone(), (i / 2) as u64 + 1))
            .collect();

        for _ in 0..200 {
            let mut warm = base.clone();
            group_bursts(&mut warm, |result| bursts.get(&result.path).copied());
        }

        let mut samples = Vec::with_capacity(20_000);
        for _ in 0..20_000 {
            let mut results = base.clone();
            let start = std::time::Instant::now();
            group_bursts(&mut results, |result| bursts.get(&result.path).copied());
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        let p50 = samples[samples.len() / 2];
        let p95 = samples[samples.len() * 95 / 100];
        eprintln!("burst grouping (50 results, release): p50 {p50} ns, p95 {p95} ns");
        assert!(p50 < 100_000, "grouping must stay under 0.1 ms");
    }

    #[test]
    fn burst_grouping_collapses_images_and_ignores_non_images() {
        let mut results = vec![
            fake_result("/shots/a.png"),
            fake_result("/shots/b.png"),
            fake_result("/shots/c.png"),
            fake_result("/shots/d.png"),
            fake_result("/docs/report.pdf"),
        ];
        let bursts = HashMap::from([
            ("/shots/a.png".to_string(), 10u64),
            ("/shots/b.png".to_string(), 10u64),
            ("/shots/c.png".to_string(), 10u64),
            ("/shots/d.png".to_string(), 20u64),
            ("/docs/report.pdf".to_string(), 10u64),
        ]);
        group_bursts(&mut results, |result| bursts.get(&result.path).copied());

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].path, "/shots/a.png");
        assert_eq!(results[0].group_count, 3);
        assert_eq!(
            results[0].group_paths,
            vec!["/shots/b.png".to_string(), "/shots/c.png".to_string()]
        );
        assert_eq!(results[1].path, "/shots/d.png");
        assert_eq!(results[1].group_count, 1);
        assert_eq!(results[2].path, "/docs/report.pdf");
        assert_eq!(results[2].group_count, 1);
    }

    #[test]
    fn sort_order_parses_known_values() {
        assert_eq!(SortOrder::parse("newest"), SortOrder::Newest);
        assert_eq!(SortOrder::parse("Oldest"), SortOrder::Oldest);
        assert_eq!(SortOrder::parse("largest"), SortOrder::Largest);
        assert_eq!(SortOrder::parse("name"), SortOrder::Name);
        assert_eq!(SortOrder::parse("nonsense"), SortOrder::Best);
    }

    use crate::indexer::metadata_db::MetadataDb;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempIndex {
        index: Option<ContentIndex>,
        dir: PathBuf,
    }

    impl Drop for TempIndex {
        fn drop(&mut self) {
            self.index.take();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_index(tag: &str) -> TempIndex {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-search-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        let index = ContentIndex::new(&dir).expect("create content index");
        TempIndex {
            index: Some(index),
            dir,
        }
    }

    fn entry(id: u64, path: &str, name: &str) -> FileEntry {
        FileEntry {
            id,
            path: PathBuf::from(path),
            name: name.to_string(),
            extension: Some("rs"),
            size: 10,
            modified: 0,
            is_dir: false,
            dev: 0,
            ino: 0,
            char_bloom: 0,
            content_pending: false,
        }
    }

    #[test]
    fn fast_response_never_reads_or_returns_snippets() {
        let temp = temp_index("fast-snippets");
        let index = temp.index.as_ref().unwrap();
        let path = "/tmp/kensaku/fast/notes.txt";
        let name = "notes.txt";
        index
            .add_or_update(
                Path::new(path),
                name,
                "needle in a haystack",
                0,
                0,
                Some("txt"),
            )
            .expect("add document");
        index.commit().unwrap();

        let db = MetadataDb::open_in_dir(&temp.dir).expect("open metadata db");
        db.upsert(&entry(1, path, name)).unwrap();
        let metadata = MetadataStore::with_db(db);

        let response = SearchDispatcher::search_response_fast(
            "needle",
            &metadata,
            &temp.index,
            None,
            false,
            10,
            0,
        );

        assert!(!response.results.is_empty());
        assert!(
            response
                .results
                .iter()
                .all(|result| result.snippet.is_none()),
            "the fast search path must not attach snippets"
        );
    }

    #[test]
    fn search_collapses_a_screenshot_burst_from_metadata() {
        let temp = temp_index("burst-search");
        let db = MetadataDb::open_in_dir(&temp.dir).expect("open metadata db");
        let text = "header\nalpha one\nbeta two\ngamma three\ndelta four";
        let mut paths = Vec::new();
        for (id, name) in [(1, "shot-a.png"), (2, "shot-b.png")] {
            let path = temp.dir.join(name);
            let mut item = entry(id, &path.to_string_lossy(), name);
            item.extension = Some("png");
            item.char_bloom = crate::search::fuzzy::compute_char_mask(name);
            db.upsert(&item).expect("insert image");
            db.set_image_meta(&path, 800, 600, 0)
                .expect("image metadata");
            db.set_image_text_sig(&path, &crate::indexer::burst::line_signature(text))
                .expect("image signature");
            db.assign_burst_for_path(&path).expect("assign burst");
            paths.push(path.to_string_lossy().to_string());
        }

        let metadata = MetadataStore::with_db(db);
        let response =
            SearchDispatcher::search_response_fast("shot", &metadata, &None, None, false, 10, 0);

        assert_eq!(
            response.results.len(),
            1,
            "burst is one row: {:?}",
            response.results
        );
        assert_eq!(response.results[0].group_count, 2);
        assert_eq!(response.results[0].group_paths.len(), 1);
        assert!(paths.contains(&response.results[0].path));
        assert!(paths.contains(&response.results[0].group_paths[0]));
    }

    #[test]
    fn folded_content_runs_only_after_exact_shortfall() {
        let temp = temp_index("folded-search");
        let index = temp.index.as_ref().unwrap();
        let path = "/tmp/kensaku/folded/donuts.txt";
        let name = "donuts.txt";
        index
            .add_or_update(
                Path::new(path),
                name,
                "Visit donutsmp.net for details",
                0,
                0,
                Some("txt"),
            )
            .expect("add document");
        index.commit().unwrap();

        let db = MetadataDb::open_in_dir(&temp.dir).expect("open metadata db");
        db.upsert(&entry(1, path, name)).unwrap();
        let metadata = MetadataStore::with_db(db);

        let folded = SearchDispatcher::search_response_fast(
            "nutsmp",
            &metadata,
            &temp.index,
            None,
            false,
            10,
            0,
        );
        assert_eq!(folded.results.len(), 1);
        assert_eq!(folded.results[0].path, path);
        assert_eq!(folded.results[0].match_type, MatchType::ContentFolded);

        // Exact hits fill the limit, so the folded tier must not run/append.
        let exact = SearchDispatcher::search_response_fast(
            "donutsmp",
            &metadata,
            &temp.index,
            None,
            false,
            1,
            0,
        );
        assert_eq!(exact.results.len(), 1);
        assert_eq!(exact.results[0].match_type, MatchType::Content);
    }

    #[test]
    fn exhaustive_results_ignore_display_limit() {
        let temp = temp_index("exhaustive");
        let index = temp.index.as_ref().unwrap();
        let mut entries = Vec::new();
        let mut path_index = HashMap::new();
        let mut candidate_indices = Vec::new();

        for i in 0..12 {
            let path = format!("/tmp/kensaku/e/file-{:02}.rs", i);
            let name = format!("file-{:02}.rs", i);
            index
                .add_or_update(Path::new(&path), &name, "shared needle", 0, 0, Some("rs"))
                .expect("add document");
            let idx = entries.len();
            entries.push(entry(i as u64 + 1, &path, &name));
            path_index.insert(PathBuf::from(&path), idx);
            candidate_indices.push(idx);
        }
        index.commit().unwrap();

        let candidate_set = CandidateSet::new(&candidate_indices, entries.len());
        let metadata = MetadataStore::without_db();
        let mut hints = Vec::new();
        let results = exhaustive_results(
            &temp.index,
            &entries,
            &path_index,
            &metadata,
            &candidate_set,
            &candidate_indices,
            &[(FieldKind::Content, "needle")],
            &[],
            EXHAUSTIVE_LIMIT,
            &mut hints,
            None,
        );

        // A normal display limit (e.g. 5) must not apply here.
        assert_eq!(results.len(), 12);
        assert!(results.len() > 5);
        let sorted = {
            let mut paths = results.iter().map(|r| r.path.clone()).collect::<Vec<_>>();
            paths.sort();
            paths
        };
        assert_eq!(
            results.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
            sorted
        );
        assert!(!hints.iter().any(|h| h.contains("capped")));
    }

    #[test]
    fn depth_penalty_is_bounded_and_does_not_dominate_relevance() {
        let now = 1_700_000_000;
        let deep_path = Path::new("/tmp/k/a/b/c/d/e/f/g/h/i/j/k/deep.rs");
        let shallow_path = Path::new("/tmp/k/shallow.rs");
        let deep = entry(1, deep_path.to_str().unwrap(), "deep.rs");
        let shallow = entry(2, shallow_path.to_str().unwrap(), "shallow.rs");

        // Equal relevance and metadata: only depth differs, so the shallower
        // path must win.
        let deep_score = finalize_score(0.5, 0.5, &deep, deep_path, "zzz", now, (0, 0));
        let shallow_score = finalize_score(0.5, 0.5, &shallow, shallow_path, "zzz", now, (0, 0));
        assert!(
            shallow_score > deep_score,
            "shallow {shallow_score} should beat deep {deep_score}"
        );

        // A very deep but highly relevant file still beats a shallow weak one.
        let relevant_deep = entry(3, deep_path.to_str().unwrap(), "alpha.rs");
        let weak_shallow = entry(4, shallow_path.to_str().unwrap(), "other.rs");
        let relevant_score =
            finalize_score(0.9, 1.0, &relevant_deep, deep_path, "alpha", now, (0, 0));
        let weak_score =
            finalize_score(0.1, 1.0, &weak_shallow, shallow_path, "alpha", now, (0, 0));
        assert!(
            relevant_score > weak_score,
            "relevant deep {relevant_score} should beat weak shallow {weak_score}"
        );
        assert!(relevant_score >= 0.0);

        // Scores never go negative, even for extremely deep unranked files.
        let very_deep = Path::new("/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z.rs");
        let bottom = finalize_score(0.0, 1.0, &deep, very_deep, "", now, (0, 0));
        assert_eq!(bottom, 0.0);
    }

    #[test]
    fn semantic_floor_drops_weak_semantic_only_hits() {
        use crate::embed::Unit;
        use crate::indexer::metadata_db::MetadataDb;

        let model = ModelRegistry::global();
        let query_vec = model.embed_to_i8("needle");

        // Build a vector with a deliberately small positive cosine (~0.07):
        // above the scanner's 0.05 prefilter, below either semantic floor.
        let (idx, value) = query_vec
            .iter()
            .enumerate()
            .max_by_key(|(_, value)| value.unsigned_abs())
            .map(|(idx, value)| (idx, *value))
            .expect("non-empty embedding");
        assert_ne!(value, 0, "embedding should have a non-zero component");
        let scaled = (1200.0 / f32::from(value.abs().max(1)))
            .round()
            .clamp(1.0, 127.0) as i8;
        let mut weak_vec = vec![0i8; query_vec.len()];
        weak_vec[idx] = scaled * value.signum();

        let mut units = UnitStore::new(model.dim);
        units.add_unit(
            Unit {
                id: 0,
                node_id: 1,
                byte_start: 0,
                byte_end: 10,
                model_id: model.model_id,
            },
            &query_vec,
        );
        units.add_unit(
            Unit {
                id: 1,
                node_id: 2,
                byte_start: 0,
                byte_end: 10,
                model_id: model.model_id,
            },
            &weak_vec,
        );
        assert_eq!(
            ExactScanner::scan(&query_vec, &units, None, 10).len(),
            2,
            "the weak vector must pass the scanner prefilter"
        );

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-semantic-floor-{}-{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = MetadataDb::open_in_dir(&dir).expect("open metadata db");
        db.upsert(&entry(1, "/tmp/kensaku/sem/strong.txt", "strong.txt"))
            .unwrap();
        db.upsert(&entry(2, "/tmp/kensaku/sem/weak.txt", "weak.txt"))
            .unwrap();
        let metadata = MetadataStore::with_db(db);

        let response = SearchDispatcher::search_response(
            "needle",
            &metadata,
            &None,
            Some(&units),
            true,
            10,
            0,
        );

        assert_eq!(
            response.results.len(),
            1,
            "weak semantic-only hit should be dropped: {:?}",
            response
                .results
                .iter()
                .map(|result| (&result.path, result.score))
                .collect::<Vec<_>>()
        );
        assert!(response.results[0].path.ends_with("strong.txt"));
        assert!(response.results[0].score >= 0.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_search_respects_semantic_enabled_and_model_id() {
        use crate::embed::Unit;
        use crate::indexer::metadata_db::MetadataDb;

        let model = ModelRegistry::global();
        let query_vec = model.embed_to_i8("needle");

        let mut units = UnitStore::new(model.dim);
        // Unit 1: active model id
        units.add_unit(
            Unit {
                id: 0,
                node_id: 1,
                byte_start: 0,
                byte_end: 10,
                model_id: model.model_id,
            },
            &query_vec,
        );
        // Unit 2: old/incompatible model id
        units.add_unit(
            Unit {
                id: 1,
                node_id: 2,
                byte_start: 0,
                byte_end: 10,
                model_id: model.model_id + 999,
            },
            &query_vec,
        );

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-semantic-model-id-{}-{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = MetadataDb::open_in_dir(&dir).expect("open metadata db");
        db.upsert(&entry(1, "/tmp/kensaku/sem/active.txt", "active.txt"))
            .unwrap();
        db.upsert(&entry(
            2,
            "/tmp/kensaku/sem/incompatible.txt",
            "incompatible.txt",
        ))
        .unwrap();
        let metadata = MetadataStore::with_db(db);

        // 1. With semantic_enabled = true: only active model unit matches (node 1)
        let response = SearchDispatcher::search_response(
            "needle",
            &metadata,
            &None,
            Some(&units),
            true,
            10,
            0,
        );
        assert_eq!(response.results.len(), 1);
        assert!(response.results[0].path.ends_with("active.txt"));

        // 2. With semantic_enabled = false: semantic search is skipped entirely
        let response_disabled = SearchDispatcher::search_response(
            "needle",
            &metadata,
            &None,
            Some(&units),
            false,
            10,
            0,
        );
        assert!(
            response_disabled.results.is_empty(),
            "when semantic_enabled=false, semantic-only results must not be returned"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn visual_search_tier_fused_into_rrf() {
        use crate::embed::visual::{MockVisualEncoder, VISUAL_DIM, VisualEncoder, VisualStore};
        use crate::indexer::metadata_db::MetadataDb;

        let encoder = MockVisualEncoder;
        let query_text = "cap";
        let target_vec = encoder.embed_text(query_text);

        let mut visual_store = VisualStore::new(VISUAL_DIM, encoder.model_id());
        // Node 1: Visual-only generic image
        visual_store.add_or_update(1, 0x1111, &target_vec);
        // Node 2: Lexical + Visual image
        visual_store.add_or_update(2, 0x2222, &target_vec);
        // Node 3: Non-image file erroneously with visual vector
        visual_store.add_or_update(3, 0x3333, &target_vec);
        // Node 4: Weak vector below cosine floor
        let inverted_vec: Vec<i8> = target_vec.iter().map(|&v| -v).collect();
        visual_store.add_or_update(4, 0x4444, &inverted_vec);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-visual-rrf-{}-{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = MetadataDb::open_in_dir(&dir).expect("open metadata db");

        let mut e1 = entry(1, "/tmp/kensaku/vis/IMG_0001.png", "IMG_0001.png");
        e1.char_bloom = crate::search::fuzzy::compute_char_mask(&e1.name);
        db.upsert(&e1).unwrap();

        let mut e2 = entry(2, "/tmp/kensaku/vis/red_cap.png", "red_cap.png");
        e2.char_bloom = crate::search::fuzzy::compute_char_mask(&e2.name);
        db.upsert(&e2).unwrap();

        let mut e3 = entry(3, "/tmp/kensaku/vis/document.txt", "document.txt");
        e3.char_bloom = crate::search::fuzzy::compute_char_mask(&e3.name);
        db.upsert(&e3).unwrap();

        let mut e4 = entry(4, "/tmp/kensaku/vis/noise.jpg", "noise.jpg");
        e4.char_bloom = crate::search::fuzzy::compute_char_mask(&e4.name);
        db.upsert(&e4).unwrap();

        let metadata = MetadataStore::with_db(db);

        let response = SearchDispatcher::search_response_with_visual(
            query_text,
            &metadata,
            &None,
            None,
            Some(&visual_store),
            false,
            10,
            0,
        );

        // 1. Weak vector node 4 dropped by floor (< 0.20)
        assert!(
            response
                .results
                .iter()
                .all(|r| !r.path.ends_with("noise.jpg")),
            "noise.jpg should be dropped by visual cosine floor"
        );

        // 2. Non-image file node 3 must never be tagged as MatchType::Visual
        for r in &response.results {
            if r.path.ends_with("document.txt") {
                assert_ne!(
                    r.match_type,
                    MatchType::Visual,
                    "non-image file document.txt must never be MatchType::Visual"
                );
            }
        }

        // 3. Node 1 (IMG_0001.png) matched visually
        let img1 = response
            .results
            .iter()
            .find(|r| r.path.ends_with("IMG_0001.png"))
            .expect("IMG_0001.png should be found via visual tier");
        assert_eq!(img1.match_type, MatchType::Visual);

        // 4. Node 2 (red_cap.png) matched both lexically and visually, outranking visual-only
        let img2 = response
            .results
            .iter()
            .find(|r| r.path.ends_with("red_cap.png"))
            .expect("red_cap.png should be found");
        assert_eq!(img2.match_type, MatchType::Both);
        assert!(
            img2.score >= img1.score,
            "lexical + visual hit (score={}) must outrank or tie visual-only hit (score={})",
            img2.score,
            img1.score
        );

        let _ = fs::remove_dir_all(&dir);
    }
    fn facet_fixture(size: usize) -> Vec<SearchResult> {
        let now = 1_700_000_000u64;
        (0..size)
            .map(|index| {
                let extension = ["pdf", "png", "rs", "md", "mp4", "zip"][index % 6];
                SearchResult {
                    path: format!(
                        "/corpus/d{:04}/file{:07}.{}",
                        index % 1000,
                        index,
                        extension
                    ),
                    filename: format!("file{:07}.{}", index, extension),
                    snippet: None,
                    score: 1.0,
                    file_size: (index as u64 % 200) * 1_048_576,
                    modified: now.saturating_sub((index as u64 % 400) * 86_400),
                    is_dir: false,
                    match_type: MatchType::FileName,
                    best_range: None,
                    group_count: 1,
                    group_paths: Vec::new(),
                }
            })
            .collect()
    }

    #[test]
    fn facet_counts_cover_the_full_set_and_stay_bounded() {
        let results = facet_fixture(100_000);
        let started = std::time::Instant::now();
        let facets = compute_facets(&results, 1_700_000_000);
        let elapsed = started.elapsed();
        assert_eq!(facets.all, 100_000);
        assert_eq!(facets.types.values().sum::<usize>(), 100_000);
        assert_eq!(facets.time.values().sum::<usize>(), 100_000);
        assert_eq!(facets.size.values().sum::<usize>(), 100_000);
        // Generous debug-mode guard; the release budget is measured by
        // `examples/facet_bench.rs` and recorded in BENCHMARKS.md.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "facets at 100k took {elapsed:?}"
        );
    }

    #[test]
    #[ignore = "manual 1M-result facet latency check"]
    fn facet_counts_1m_stay_bounded() {
        let results = facet_fixture(1_000_000);
        let started = std::time::Instant::now();
        let facets = compute_facets(&results, 1_700_000_000);
        let elapsed = started.elapsed();
        assert_eq!(facets.all, 1_000_000);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "facets at 1M took {elapsed:?}"
        );
    }
    #[test]
    fn pushdown_uses_only_positive_ext_and_size() {
        let filters = vec![
            Filter::Not(Box::new(Filter::Ext("pdf".to_string()))),
            Filter::Size(RangeOp::Gt(10)),
            Filter::Ext("rs".to_string()),
            Filter::Ext("ts".to_string()),
            Filter::Size(RangeOp::Lt(99)),
        ];
        let pushdown = content_pushdown(&filters);
        assert_eq!(pushdown.ext.as_deref(), Some("rs"));
        assert_eq!(pushdown.size, Some(RangeOp::Gt(10)));

        let only_negated = vec![Filter::Not(Box::new(Filter::Ext("pdf".to_string())))];
        assert!(content_pushdown(&only_negated).is_empty());
    }

    #[test]
    fn facet_classification_matches_bucket_tables() {
        assert_eq!(classify_extension("rs"), (3, Some(0)));
        assert_eq!(LANGUAGE_BUCKETS[0], "rust");
        assert_eq!(classify_extension("md"), (2, Some(13)));
        assert_eq!(LANGUAGE_BUCKETS[13], "markdown");
        assert_eq!(classify_extension("json"), (3, Some(14)));
        assert_eq!(classify_extension("png"), (1, None));
        assert_eq!(TYPE_BUCKETS[1], "image");
        assert_eq!(classify_extension("zip"), (5, None));
        assert_eq!(classify_extension("unknown"), (6, None));
        assert_eq!(classify_extension("pdf"), (2, None));
    }
}
