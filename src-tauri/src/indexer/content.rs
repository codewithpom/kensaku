use crate::query::ast::RangeOp;
use crate::query::parser::NEAR_MAX_DISTANCE;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, Occur, Query, QueryParser, RangeQuery, RegexQuery, TermQuery,
};
use tantivy::schema::*;
use tantivy::tokenizer::NgramTokenizer;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};

/// Converts a size predicate into Tantivy term bounds over the `size` fast
/// field.
fn size_range_terms(field: Field, range: RangeOp) -> (Bound<Term>, Bound<Term>) {
    match range {
        RangeOp::Lt(value) => (
            Bound::Unbounded,
            Bound::Excluded(Term::from_field_u64(field, value)),
        ),
        RangeOp::Lte(value) => (
            Bound::Unbounded,
            Bound::Included(Term::from_field_u64(field, value)),
        ),
        RangeOp::Gt(value) => (
            Bound::Excluded(Term::from_field_u64(field, value)),
            Bound::Unbounded,
        ),
        RangeOp::Gte(value) => (
            Bound::Included(Term::from_field_u64(field, value)),
            Bound::Unbounded,
        ),
        RangeOp::Between(low, high) => (
            Bound::Included(Term::from_field_u64(field, low)),
            Bound::Included(Term::from_field_u64(field, high)),
        ),
    }
}

/// Canonicalizes OCR/lookalike characters for the folded trigram tier.
///
/// Spaces and punctuation are preserved, so folding never joins two words.
/// The mapping is intentionally lossy in one direction: `rn → m`, `vv → w`,
/// and `cl → d` make the longer spelling match the shorter query.
pub fn fold(text: &str) -> String {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut out = String::with_capacity(lower.len());
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            'r' if chars.get(index + 1) == Some(&'n') => {
                out.push('m');
                index += 2;
                continue;
            }
            'v' if chars.get(index + 1) == Some(&'v') => {
                out.push('w');
                index += 2;
                continue;
            }
            'c' if chars.get(index + 1) == Some(&'l') => {
                out.push('d');
                index += 2;
                continue;
            }
            '0' => out.push('o'),
            '1' | 'l' | '|' => out.push('i'),
            '5' | '$' => out.push('s'),
            '8' => out.push('b'),
            other => out.push(other),
        }
        index += 1;
    }
    out
}

/// Verifies a folded candidate against the original text.
///
/// Every query term of three or more characters must occur exactly, or within
/// the OCR budget: one misread for 4–7 characters, two for 8+. Shorter terms
/// must match exactly.
pub fn verify_folded(raw: &str, query: &str) -> bool {
    let lower = raw.to_lowercase();
    let terms: Vec<&str> = query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| term.chars().count() >= 3)
        .collect();
    if terms.is_empty() {
        return false;
    }
    terms.iter().all(|term| term_matches(&lower, term))
}

fn term_matches(raw_lower: &str, term: &str) -> bool {
    let term_lower = term.to_lowercase();
    let folded_term = fold(&term_lower);
    if raw_lower.contains(&term_lower) {
        return true;
    }
    let budget = match folded_term.chars().count() {
        4..=7 => 1,
        8.. => 2,
        _ => 0,
    };
    if budget == 0 {
        // The two-character to one-character folds are exact mappings, so a
        // one-character folded term still has to match a whole folded word.
        return raw_lower
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .any(|word| fold(word) == folded_term);
    }
    let needle: Vec<char> = folded_term.chars().collect();
    raw_lower
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .filter(|word| {
            let folded_word = fold(word);
            folded_word.chars().count().abs_diff(needle.len()) <= budget
        })
        .any(|word| {
            let folded_word = fold(word);
            let hay: Vec<char> = folded_word.chars().collect();
            edit_distance_within(&needle, &hay.iter().collect::<String>(), budget)
        })
}

fn edit_distance_within(needle: &[char], haystack: &str, budget: usize) -> bool {
    let hay: Vec<char> = haystack.chars().collect();
    if needle.len().abs_diff(hay.len()) > budget {
        return false;
    }
    let mut previous: Vec<usize> = (0..=hay.len()).collect();
    for (i, needle_char) in needle.iter().enumerate() {
        let mut current = vec![i + 1; hay.len() + 1];
        let mut row_min = current[0];
        for (j, hay_char) in hay.iter().enumerate() {
            let substitution = previous[j] + usize::from(needle_char != hay_char);
            current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
            row_min = row_min.min(current[j + 1]);
        }
        if row_min > budget {
            return false;
        }
        previous = current;
    }
    previous[hay.len()] <= budget
}

/// Returns true when `text` contains a `near[x]` proximity operator.
pub fn contains_near(text: &str) -> bool {
    text.split_whitespace().any(parse_near_distance_opt)
}

fn parse_near_distance_opt(tok: &str) -> bool {
    parse_near_distance(tok).is_some()
}

fn parse_near_distance(tok: &str) -> Option<u32> {
    let lower = tok.to_ascii_lowercase();
    let rest = lower.strip_prefix("near")?;
    let rest = rest.trim();
    let inner = rest.strip_prefix('[')?;
    let digits: String = inner.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = inner[digits.len()..].trim();
    if after != "]" {
        return None;
    }
    digits.parse().ok()
}

/// Rewrites `a near[n] b` (and chains) into a Tantivy phrase query with slop:
/// `"a b"~n`. Other tokens are sanitized as usual.
///
/// A `near[d]` operator is never emitted as a term: when it cannot bind a term
/// on both sides (leading, trailing typo, doubled `a near[2] near[3] b`, where
/// the last distance wins to match the parser) it is dropped, mirroring the
/// query parser which never pushes it into the term list.
pub fn rewrite_near_in_group(input: &str) -> String {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if parse_near_distance(tokens[i]).is_some() {
            i += 1;
            continue;
        }
        // Accumulate `<term> (near[d] <term>)*` into one phrase. Sanitized
        // tokens are trimmed: characters like `+` or `*` sanitize down to
        // spaces inside a single word.
        let mut terms: Vec<String> = Vec::new();
        {
            let cleaned = sanitize_query_text(tokens[i]).trim().to_string();
            if !cleaned.is_empty() {
                terms.push(cleaned);
            }
        }
        let mut slop: u32 = 0;
        let mut bound = false;
        let mut j = i + 1;
        loop {
            // Scan a run of operators; only the last distance in a run binds.
            let mut last_op: Option<u32> = None;
            while j < tokens.len()
                && let Some(d) = parse_near_distance(tokens[j])
            {
                last_op = Some(d);
                j += 1;
            }
            let Some(d) = last_op else {
                break;
            };
            if j >= tokens.len() {
                break; // unbound trailing operator
            }
            let cleaned = sanitize_query_text(tokens[j]).trim().to_string();
            if !cleaned.is_empty() {
                slop += d;
                bound = true;
                terms.push(cleaned);
            }
            j += 1;
        }
        i = j;
        if bound && terms.len() >= 2 {
            let slop = slop.min(NEAR_MAX_DISTANCE);
            out.push(format!("\"{}\"~{}", terms.join(" "), slop));
        } else {
            out.extend(terms);
        }
    }
    out.join(" ")
}

pub struct SnippetCache {
    capacity: usize,
    map: HashMap<PathBuf, (String, String)>, // (original, lowercase)
    order: VecDeque<PathBuf>,
}

impl SnippetCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get_or_read(&mut self, path: &Path) -> Option<(&str, &str)> {
        if self.map.contains_key(path) {
            if let Some(pos) = self.order.iter().position(|p| p == path) {
                self.order.remove(pos);
            }
            self.order.push_back(path.to_path_buf());
            return self
                .map
                .get(path)
                .map(|(orig, lower)| (orig.as_str(), lower.as_str()));
        }

        let content = read_text_content(path, 32_000)?;
        let lower = content.to_lowercase();
        if self.map.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }
        self.order.push_back(path.to_path_buf());
        self.map.insert(path.to_path_buf(), (content, lower));
        self.map
            .get(path)
            .map(|(orig, lower)| (orig.as_str(), lower.as_str()))
    }
}

pub struct ContentIndex {
    index: Index,
    reader: IndexReader,
    /// `None` for read-only handles (CLI/MCP): Tantivy takes an exclusive lock
    /// while a writer exists, so a second process can only open the reader.
    writer: Option<Arc<RwLock<IndexWriter>>>,
    pub path_field: Field,
    pub filename_field: Field,
    pub content_field: Field,
    /// Folded text indexed with a 3-gram tokenizer. `None` for a read-only
    /// handle opened on the pre-K5 schema.
    pub content_folded_field: Option<Field>,
    /// Original text used to verify folded candidates. Stored but not indexed.
    pub content_raw_field: Option<Field>,
    pub modified_field: Field,
    /// Exact extension (lowercase) for filter pushdown. `None` on a legacy
    /// read-only handle.
    pub ext_field: Option<Field>,
    /// File size in bytes for range pushdown. `None` on a legacy handle.
    pub size_field: Option<Field>,
    /// True when this construction migrated an older schema and the caller
    /// must force a full content re-extraction.
    pub rebuilt: bool,
    /// Reason recorded when an index was rebuilt during migration.
    pub rebuild_reason: Option<String>,
    snippet_cache: Mutex<SnippetCache>,
}

#[derive(Debug, Clone)]
pub struct ContentMatch {
    pub path: PathBuf,
    pub filename: String,
    pub snippet: Option<String>,
    pub score: f32,
    pub modified: u64,
}

/// Which text field a BM25 group should be scoped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Search both `filename` and `content`.
    Any,
    /// Search only `filename`.
    Name,
    /// Search only `content`.
    Content,
}

/// Filters pushed into the Tantivy query before top-k collection.
///
/// Only positive clauses are represented. Type/lang/name/path and negations
/// remain metadata post-filters; pushing a positive ext/size is always safe
/// because every surviving result must satisfy it too.
#[derive(Debug, Clone, Default)]
pub struct ContentFilters {
    pub ext: Option<String>,
    pub size: Option<crate::query::ast::RangeOp>,
}

impl ContentFilters {
    pub fn is_empty(&self) -> bool {
        self.ext.is_none() && self.size.is_none()
    }
}

/// Result of a grouped BM25 search that may also include regex clauses.
#[derive(Debug, Clone, Default)]
pub struct ContentSearchOutcome {
    pub matches: Vec<ContentMatch>,
    /// Actionable messages for ignored/invalid regex patterns.
    pub hints: Vec<String>,
}

/// Upper bound on user-supplied regex patterns to avoid pathological cases.
pub const MAX_REGEX_PATTERN_LEN: usize = 1024;

/// Version marker for the content index schema.
/// v1: K5 schema (content_folded + content_raw)
/// v2: K15 schema (CJK-aware "kensaku" tokenizer on filename and content)
/// v3: ext/size fields for filter pushdown (K12/B4)
/// v4: trigram field stores term frequencies only, not positions (#129).
///     Nothing queries positions: folded search builds boolean term queries,
///     and no phrase query exists. Positions were roughly the largest single
///     contributor to index size per file.
pub const CONTENT_SCHEMA_VERSION: u32 = 4;

/// Records the schema version the schema version this index was written with.
///
/// This used to swallow its error. A failed write leaves the marker at version
/// 0, so the next launch reads a stale schema, rebuilds the whole index, and
/// re-extracts every file on every single start, silently.
fn write_schema_marker(path: &std::path::Path) {
    if let Err(error) = fs::write(path, CONTENT_SCHEMA_VERSION.to_string()) {
        log::error!(
            "[Kensaku] could not record the content schema version at {}: {}",
            path.display(),
            error
        );
    }
}

pub const SCHEMA_MARKER_FILE: &str = "content.schema";

impl ContentIndex {
    pub fn new(index_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(index_dir).map_err(|e| e.to_string())?;

        let mut schema_builder = Schema::builder();
        let path_field = schema_builder.add_text_field("path", STRING | STORED);
        let kensaku_indexing = TextFieldIndexing::default()
            .set_tokenizer("kensaku")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let filename_options = TextOptions::default()
            .set_indexing_options(kensaku_indexing.clone())
            .set_stored();
        let filename_field = schema_builder.add_text_field("filename", filename_options);
        let content_options = TextOptions::default().set_indexing_options(kensaku_indexing);
        let content_field = schema_builder.add_text_field("content", content_options);
        // Frequencies only: folded search queries individual trigram terms
        // through boolean term queries and verifies candidates against the
        // stored raw text, so positions are never read. See schema v4.
        let folded_indexing = TextFieldIndexing::default()
            .set_tokenizer("trigram")
            .set_index_option(IndexRecordOption::WithFreqs);
        let folded_field = schema_builder.add_text_field(
            "content_folded",
            TextOptions::default().set_indexing_options(folded_indexing),
        );
        let raw_field = schema_builder.add_text_field("content_raw", STORED);
        let modified_field = schema_builder.add_u64_field("modified", STORED | FAST);
        // K12/B4: exact extension and byte size, indexed/fast so ext/size
        // filters can narrow the Tantivy query before top-k collection.
        let ext_field = schema_builder.add_text_field("ext", STRING);
        let size_field = schema_builder.add_u64_field("size", FAST);
        let schema = schema_builder.build();

        let mut rebuilt = false;
        let mut rebuild_reason = None;
        let marker_path = index_dir.join(SCHEMA_MARKER_FILE);
        let index = if index_dir.join("meta.json").exists() {
            let existing = Index::open_in_dir(index_dir).map_err(|e| e.to_string())?;
            let existing_schema = existing.schema();
            let has_fields = existing_schema.get_field("content_folded").is_ok()
                && existing_schema.get_field("content_raw").is_ok();
            let has_pushdown = existing_schema.get_field("ext").is_ok()
                && existing_schema.get_field("size").is_ok();
            let marker_version: u32 = fs::read_to_string(&marker_path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let compatible = has_fields && has_pushdown && marker_version >= CONTENT_SCHEMA_VERSION;
            if compatible {
                existing
            } else {
                let reason = if !has_fields {
                    "Pre-K5 schema missing content_folded/content_raw fields"
                } else if marker_version < 2 {
                    "K15 CJK tokenizer migration (schema v1 -> v2)"
                } else if !has_pushdown {
                    "K12 filter pushdown migration (schema v2 -> v3)"
                } else if marker_version < 4 {
                    "trigram posting format change (schema v3 -> v4, #129)"
                } else {
                    "Content schema marker is stale"
                };
                rebuild_reason = Some(reason.to_string());
                log::info!("[Kensaku] Content index rebuild required: {}", reason);
                // Tantivy schemas are immutable. Preserve the old derived index
                // beside the live one and rebuild into the live path; the
                // caller marks every file for content re-extraction.
                drop(existing);
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0);
                let backup = index_dir.with_file_name(format!("index.legacy-{stamp}"));
                fs::rename(index_dir, &backup).map_err(|e| {
                    format!(
                        "Failed to move legacy content index to {}: {}",
                        backup.display(),
                        e
                    )
                })?;
                fs::create_dir_all(index_dir).map_err(|e| e.to_string())?;
                rebuilt = true;
                let new_index =
                    Index::create_in_dir(index_dir, schema.clone()).map_err(|e| e.to_string())?;
                write_schema_marker(&marker_path);
                new_index
            }
        } else {
            let new_index =
                Index::create_in_dir(index_dir, schema.clone()).map_err(|e| e.to_string())?;
            write_schema_marker(&marker_path);
            new_index
        };
        index
            .tokenizers()
            .register("kensaku", crate::indexer::tokenizer::kensaku_analyzer());
        index.tokenizers().register(
            "trigram",
            NgramTokenizer::new(3, 3, false).map_err(|e| e.to_string())?,
        );

        // A desktop daemon should not reserve a large indexing arena. Tantivy
        // requires ~15 MB per writer thread; one thread with a 16 MB arena keeps
        // the resident footprint small. Indexing is I/O-bound for a personal
        // corpus and extraction, not document insertion, is the real cost.
        let writer = index
            .writer_with_num_threads(1, 16_000_000)
            .map_err(|e| e.to_string())?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| e.to_string())?;

        Ok(Self {
            index,
            reader,
            writer: Some(Arc::new(RwLock::new(writer))),
            path_field,
            filename_field,
            content_field,
            content_folded_field: Some(folded_field),
            content_raw_field: Some(raw_field),
            modified_field,
            ext_field: Some(ext_field),
            size_field: Some(size_field),
            rebuilt,
            rebuild_reason,
            snippet_cache: Mutex::new(SnippetCache::new(32)),
        })
    }

    /// Opens an existing index for searching only.
    ///
    /// Tantivy allows a writer in the GUI process and readers in other
    /// processes at the same time, so the CLI/MCP surface uses this to read the
    /// live index without taking the exclusive writer lock.
    pub fn open_read_only(index_dir: &Path) -> Result<Self, String> {
        if !index_dir.join("meta.json").exists() {
            return Err(format!("No content index at {}", index_dir.display()));
        }

        let index = Index::open_in_dir(index_dir).map_err(|e| e.to_string())?;
        let schema = index.schema();
        let path_field = schema
            .get_field("path")
            .map_err(|e| format!("Content index is missing the 'path' field: {}", e))?;
        let filename_field = schema
            .get_field("filename")
            .map_err(|e| format!("Content index is missing the 'filename' field: {}", e))?;
        let content_field = schema
            .get_field("content")
            .map_err(|e| format!("Content index is missing the 'content' field: {}", e))?;
        let content_folded_field = schema.get_field("content_folded").ok();
        let content_raw_field = schema.get_field("content_raw").ok();
        let ext_field = schema.get_field("ext").ok();
        let size_field = schema.get_field("size").ok();
        let modified_field = schema
            .get_field("modified")
            .map_err(|e| format!("Content index is missing the 'modified' field: {}", e))?;

        // Read-only handles need the kensaku and trigram tokenizers for query parsing.
        index
            .tokenizers()
            .register("kensaku", crate::indexer::tokenizer::kensaku_analyzer());
        index.tokenizers().register(
            "trigram",
            NgramTokenizer::new(3, 3, false).map_err(|e| e.to_string())?,
        );

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| e.to_string())?;

        Ok(Self {
            index,
            reader,
            writer: None,
            path_field,
            filename_field,
            content_field,
            content_folded_field,
            content_raw_field,
            modified_field,
            ext_field,
            size_field,
            rebuilt: false,
            rebuild_reason: None,
            snippet_cache: Mutex::new(SnippetCache::new(32)),
        })
    }

    /// True when this handle can mutate the index.
    pub fn is_writable(&self) -> bool {
        self.writer.is_some()
    }

    /// Deletes every document in the index and commits.
    ///
    /// Used by the in-process rebuild (`IndexerManager::rebuild_all`) when the
    /// index directory itself could not be deleted because another handle
    /// still had it mapped (Windows): purging logically is equivalent, and
    /// Tantivy reclaims the space at the next segment merge.
    pub fn truncate(&mut self) -> Result<(), String> {
        {
            let mut writer = self.writer()?.write();
            writer.delete_all_documents().map_err(|e| e.to_string())?;
            writer.commit().map_err(|e| e.to_string())?;
        }
        // Cached snippets belong to the deleted documents.
        self.snippet_cache = Mutex::new(SnippetCache::new(32));
        Ok(())
    }

    fn writer(&self) -> Result<&Arc<RwLock<IndexWriter>>, String> {
        self.writer
            .as_ref()
            .ok_or_else(|| "index is read-only".to_string())
    }

    /// Number of documents visible to the reader (including deleted-but-not-
    /// yet-merged documents, which is what Tantivy reports).
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn add_or_update(
        &self,
        path: &Path,
        filename: &str,
        content: &str,
        modified: u64,
        size: u64,
        extension: Option<&str>,
    ) -> Result<(), String> {
        let path_str = path.to_string_lossy().to_string();
        let term = Term::from_field_text(self.path_field, &path_str);

        let writer = self.writer()?.write();
        writer.delete_term(term);

        let mut doc = doc!(
            self.path_field => path_str,
            self.filename_field => filename.to_string(),
            self.content_field => content.to_string(),
            self.modified_field => modified,
        );
        if let Some(field) = self.content_folded_field {
            doc.add_text(field, fold(content));
        }
        if let Some(field) = self.content_raw_field {
            doc.add_text(field, content);
        }
        if let Some(field) = self.ext_field
            && let Some(extension) = extension
        {
            doc.add_text(field, extension.to_ascii_lowercase());
        }
        if let Some(field) = self.size_field {
            doc.add_u64(field, size);
        }
        writer.add_document(doc).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn remove(&self, path: &Path) -> Result<(), String> {
        let path_str = path.to_string_lossy().to_string();
        let term = Term::from_field_text(self.path_field, &path_str);
        let writer = self.writer()?.write();
        writer.delete_term(term);
        Ok(())
    }

    /// Queues deletion of many documents before the next commit.
    ///
    /// Used by the crawl to drop documents for files that disappeared from
    /// disk. Unlike [`Self::purge_stale`] this never scans the whole index.
    pub fn remove_many(&self, paths: &[PathBuf]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let writer = self.writer()?.write();
        for path in paths {
            let path_str = path.to_string_lossy();
            let term = Term::from_field_text(self.path_field, &path_str);
            writer.delete_term(term);
        }
        Ok(())
    }

    /// Deletes every indexed document whose path is not present in
    /// `valid_paths`, except documents under `preserved_roots`.
    ///
    /// This is used after a full crawl to remove documents that belong to
    /// files deleted while the application was not running (or to watched
    /// directories removed from the configuration). `preserved_roots` are
    /// configured roots that were unavailable during the crawl (unplugged
    /// drive, unmounted share); their documents are kept so a transient outage
    /// cannot wipe the index. Returns the number of documents purged.
    pub fn purge_stale(
        &self,
        valid_paths: &HashSet<PathBuf>,
        preserved_roots: &[PathBuf],
    ) -> Result<usize, String> {
        // Read-only handles must fail before doing the (expensive) scan.
        self.writer()?;

        // Documents store `to_string_lossy()` paths, so compare in the same
        // representation to avoid purging valid non-UTF-8 paths.
        let valid_lossy: HashSet<String> = valid_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();

        let searcher = self.reader.searcher();
        let doc_addresses = searcher
            .search(&AllQuery, &DocSetCollector)
            .map_err(|e| format!("Failed to scan content index: {}", e))?;

        let mut stale_paths: Vec<String> = Vec::new();
        for doc_address in doc_addresses {
            let Ok(doc) = searcher.doc::<TantivyDocument>(doc_address) else {
                continue;
            };
            let Some(path_str) = doc.get_first(self.path_field).and_then(|f| f.as_str()) else {
                continue;
            };
            if valid_lossy.contains(path_str) {
                continue;
            }
            let path = Path::new(path_str);
            if preserved_roots.iter().any(|root| path.starts_with(root)) {
                continue;
            }
            stale_paths.push(path_str.to_string());
        }

        if stale_paths.is_empty() {
            return Ok(0);
        }

        {
            let writer = self.writer()?.write();
            for path_str in &stale_paths {
                let term = Term::from_field_text(self.path_field, path_str);
                writer.delete_term(term);
            }
        }

        self.commit()?;
        Ok(stale_paths.len())
    }

    pub fn commit(&self) -> Result<(), String> {
        let mut writer = self.writer()?.write();
        writer.commit().map_err(|e| e.to_string())?;
        self.reader.reload().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn search(
        &self,
        query_str: &str,
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Vec<ContentMatch> {
        self.search_groups(&[(FieldKind::Any, query_str)], limit, path_prefix)
    }

    /// Field-aware BM25 search. Each group is ANDed with the others; within a
    /// group multi-word input narrows (conjunction by default).
    pub fn search_groups(
        &self,
        groups: &[(FieldKind, &str)],
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Vec<ContentMatch> {
        self.search_groups_with_regex(groups, &[], limit, path_prefix)
            .matches
    }

    /// Like [`Self::search_groups`], plus optional `re:` patterns.
    ///
    /// Regex clauses are `Should` clauses and are only added when a pattern was
    /// explicitly supplied by the caller (the query parser only sets the regex
    /// flag for `re:` tokens). Plain term groups and regex clauses are combined
    /// so that at least one of them must match.
    pub fn search_groups_with_regex(
        &self,
        groups: &[(FieldKind, &str)],
        regex_patterns: &[&str],
        limit: usize,
        path_prefix: Option<&str>,
    ) -> ContentSearchOutcome {
        self.search_groups_filtered(
            groups,
            regex_patterns,
            limit,
            path_prefix,
            &ContentFilters::default(),
        )
    }

    /// Like [`Self::search_groups_with_regex`], with positive ext/size filters
    /// pushed into the Tantivy query before top-k collection.
    pub fn search_groups_filtered(
        &self,
        groups: &[(FieldKind, &str)],
        regex_patterns: &[&str],
        limit: usize,
        path_prefix: Option<&str>,
        filters: &ContentFilters,
    ) -> ContentSearchOutcome {
        let mut hints = Vec::new();

        let mut plain_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for &(kind, text) in groups {
            let sanitized = if contains_near(text) {
                rewrite_near_in_group(text)
            } else {
                sanitize_query_text(text)
            };
            if sanitized.trim().is_empty() {
                continue;
            }
            let fields = match kind {
                FieldKind::Any => vec![self.filename_field, self.content_field],
                FieldKind::Name => vec![self.filename_field],
                FieldKind::Content => vec![self.content_field],
            };
            let mut parser = QueryParser::for_index(&self.index, fields);
            parser.set_conjunction_by_default();
            // `sanitize_query_text` removes every parser metacharacter, so this
            // should always succeed; skip the group if it somehow does not.
            if let Ok(parsed) = parser.parse_query(&sanitized) {
                plain_clauses.push((Occur::Must, parsed));
            }
        }

        let mut regex_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for &pattern in regex_patterns {
            if pattern.is_empty() {
                continue;
            }
            if pattern.chars().count() > MAX_REGEX_PATTERN_LEN {
                hints.push(format!(
                    "Regex pattern too long (max {} characters); ignored",
                    MAX_REGEX_PATTERN_LEN
                ));
                continue;
            }

            let mut field_queries: Vec<RegexQuery> = Vec::new();
            let mut failed = false;
            for field in [self.content_field, self.filename_field] {
                match RegexQuery::from_pattern(pattern, field) {
                    Ok(query) => field_queries.push(query),
                    Err(err) => {
                        hints.push(format!("Invalid regex '{}': {}", pattern, err));
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                for query in field_queries {
                    regex_clauses.push((Occur::Should, Box::new(query)));
                }
            }
        }

        let plain_query: Option<Box<dyn Query>> = if plain_clauses.is_empty() {
            None
        } else {
            Some(Box::new(BooleanQuery::new(plain_clauses)))
        };
        let regex_query: Option<Box<dyn Query>> = if regex_clauses.is_empty() {
            None
        } else {
            Some(Box::new(BooleanQuery::new(regex_clauses)))
        };

        let query: Option<Box<dyn Query>> = match (plain_query, regex_query) {
            (None, None) => None,
            (Some(plain), None) => Some(plain),
            (None, Some(regex)) => Some(regex),
            (Some(plain), Some(regex)) => Some(Box::new(BooleanQuery::new(vec![
                (Occur::Should, plain),
                (Occur::Should, regex),
            ]))),
        };

        let matches = match query {
            Some(q) => self.run_query(q.as_ref(), limit, path_prefix, filters),
            None => Vec::new(),
        };

        ContentSearchOutcome { matches, hints }
    }

    /// Folded trigram search used only when exact BM25 left a shortfall.
    ///
    /// Candidates are retrieved from the 3-gram field, then verified against
    /// the stored raw text with a length-dependent edit budget. This rejects
    /// mass false positives from broad trigrams.
    pub fn search_folded(
        &self,
        query: &str,
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Vec<ContentMatch> {
        self.search_folded_filtered(query, limit, path_prefix, &ContentFilters::default())
    }

    /// Folded search with positive ext/size filters pushed down.
    pub fn search_folded_filtered(
        &self,
        query: &str,
        limit: usize,
        path_prefix: Option<&str>,
        filters: &ContentFilters,
    ) -> Vec<ContentMatch> {
        let (Some(folded_field), Some(raw_field)) =
            (self.content_folded_field, self.content_raw_field)
        else {
            return Vec::new();
        };
        if limit == 0 {
            return Vec::new();
        }

        let folded = fold(query);
        let mut term_groups: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for term in folded.split(|ch: char| !ch.is_alphanumeric()) {
            let chars: Vec<char> = term.chars().collect();
            if chars.len() < 3 {
                continue;
            }
            let mut grams: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            let mut seen = HashSet::new();
            for window in chars.windows(3) {
                let gram: String = window.iter().collect();
                if !seen.insert(gram.clone()) {
                    continue;
                }
                grams.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(folded_field, &gram),
                        IndexRecordOption::WithFreqs,
                    )),
                ));
            }
            if !grams.is_empty() {
                term_groups.push((Occur::Must, Box::new(BooleanQuery::new(grams))));
            }
        }
        if term_groups.is_empty() {
            return Vec::new();
        }

        let boolean_query: Box<dyn Query> = Box::new(BooleanQuery::new(term_groups));
        let effective_query = self.effective_query(boolean_query, path_prefix, filters);

        let searcher = self.reader.searcher();
        let collector = TopDocs::with_limit(limit.saturating_mul(2).max(1)).order_by_score();
        let Ok(top_docs) = searcher.search(effective_query.as_ref(), &collector) else {
            return Vec::new();
        };

        let mut results = Vec::new();
        for (score, address) in top_docs {
            let Ok(document) = searcher.doc::<TantivyDocument>(address) else {
                continue;
            };
            let path_str = document
                .get_first(self.path_field)
                .and_then(|field| field.as_str())
                .unwrap_or("")
                .to_string();
            if path_str.is_empty() {
                continue;
            }
            if let Some(prefix) = path_prefix
                && !path_str.starts_with(prefix)
            {
                continue;
            }
            let raw = document
                .get_first(raw_field)
                .and_then(|field| field.as_str())
                .unwrap_or("");
            if !verify_folded(raw, query) {
                continue;
            }
            let filename = document
                .get_first(self.filename_field)
                .and_then(|field| field.as_str())
                .unwrap_or("")
                .to_string();
            let modified = document
                .get_first(self.modified_field)
                .and_then(|field| field.as_u64())
                .unwrap_or(0);
            results.push(ContentMatch {
                path: PathBuf::from(path_str),
                filename,
                snippet: None,
                score,
                modified,
            });
            if results.len() >= limit {
                break;
            }
        }
        results
    }

    /// Combines a base query with the pushed-down scope and ext/size filters.
    fn effective_query(
        &self,
        base: Box<dyn Query>,
        path_prefix: Option<&str>,
        filters: &ContentFilters,
    ) -> Box<dyn Query> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, base)];
        if let Some(prefix) = path_prefix.filter(|prefix| !prefix.is_empty()) {
            let pattern = format!("(?s)^{}.*", regex::escape(prefix));
            if let Ok(path_query) = RegexQuery::from_pattern(&pattern, self.path_field) {
                clauses.push((Occur::Must, Box::new(path_query)));
            }
        }
        clauses.extend(self.pushdown_clauses(filters));
        if clauses.len() == 1 {
            clauses.pop().expect("one clause").1
        } else {
            Box::new(BooleanQuery::new(clauses))
        }
    }

    /// Builds `Must` clauses for the filters that have a stored field.
    fn pushdown_clauses(&self, filters: &ContentFilters) -> Vec<(Occur, Box<dyn Query>)> {
        let mut clauses = Vec::new();
        if let (Some(field), Some(ext)) = (self.ext_field, filters.ext.as_deref()) {
            let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
            if !ext.is_empty() {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, &ext),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                ));
            }
        }
        if let (Some(field), Some(range)) = (self.size_field, filters.size) {
            let (lower, upper) = size_range_terms(field, range);
            clauses.push((
                Occur::Must,
                Box::new(RangeQuery::new(lower, upper)) as Box<dyn Query>,
            ));
        }
        clauses
    }

    fn run_query(
        &self,
        query: &dyn Query,
        limit: usize,
        path_prefix: Option<&str>,
        filters: &ContentFilters,
    ) -> Vec<ContentMatch> {
        if limit == 0 {
            return Vec::new();
        }

        let searcher = self.reader.searcher();
        let collector = TopDocs::with_limit(limit).order_by_score();

        // Push the scope and ext/size filters down into the Tantivy query:
        // only documents that can survive are scored at all.
        let effective_query = self.effective_query(query.box_clone(), path_prefix, filters);

        let top_docs = match searcher.search(effective_query.as_ref(), &collector) {
            Ok(docs) => docs,
            Err(_) => return Vec::new(),
        };

        let mut results = Vec::new();

        for (score, doc_address) in top_docs {
            if let Ok(retrieved_doc) = searcher.doc::<TantivyDocument>(doc_address) {
                let path_str = retrieved_doc
                    .get_first(self.path_field)
                    .and_then(|f| f.as_str())
                    .unwrap_or("")
                    .to_string();

                // Apply path prefix filter (pushed from scope)
                if let Some(prefix) = path_prefix
                    && !path_str.starts_with(prefix)
                {
                    continue;
                }

                let filename = retrieved_doc
                    .get_first(self.filename_field)
                    .and_then(|f| f.as_str())
                    .unwrap_or("")
                    .to_string();

                let modified = retrieved_doc
                    .get_first(self.modified_field)
                    .and_then(|f| f.as_u64())
                    .unwrap_or(0);

                if !path_str.is_empty() {
                    let path = PathBuf::from(&path_str);
                    results.push(ContentMatch {
                        path,
                        filename,
                        snippet: None,
                        score,
                        modified,
                    });
                }
            }
        }

        results
    }

    /// Returns the stored original text for one path, when the schema has it.
    /// Used by the idle screenshot-burst backfill; never runs extraction.
    pub fn get_stored_text(&self, path: &Path) -> Option<String> {
        let field = self.content_raw_field?;
        let searcher = self.reader.searcher();
        let term = Term::from_field_text(self.path_field, &path.to_string_lossy());
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(1).order_by_score())
            .ok()?;
        let (_, address) = top_docs.first()?;
        let document = searcher.doc::<TantivyDocument>(*address).ok()?;
        document
            .get_first(field)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    pub fn get_snippet(&self, path: &Path, query: &str) -> Option<String> {
        let mut cache = self.snippet_cache.lock();
        let (content, content_lower) = cache.get_or_read(path)?;
        Self::extract_snippet_from_text_cached(content, content_lower, query)
    }

    /// Cheap, extraction-free snippet for query-time callers.
    ///
    /// Plain text/code/config files are read directly (at most 64 KB). Every
    /// other format (PDF, Office, RTF, images, ...) is served only from the
    /// extraction cache, so this never runs lopdf, unzip or tesseract on the
    /// query path. A cold cache yields `None`; the UI fetches snippets
    /// asynchronously via the `get_snippets` command instead.
    pub fn get_snippet_fast(&self, path: &Path, query: &str) -> Option<String> {
        snippet_for_path(path, query)
    }

    pub fn extract_snippet_from_text_cached(
        content: &str,
        content_lower: &str,
        query: &str,
    ) -> Option<String> {
        let query_lower = query.to_lowercase();
        let terms: Vec<&str> = query_lower.split_whitespace().collect();
        if terms.is_empty() {
            return None;
        }

        let mut first_match_idx = None;

        for term in &terms {
            if let Some(pos) = content_lower.find(term) {
                first_match_idx = match first_match_idx {
                    Some(cur) => Some(std::cmp::min(cur, pos)),
                    None => Some(pos),
                };
            }
        }

        let match_idx = first_match_idx?;
        let start = match_idx.saturating_sub(40);
        let end = std::cmp::min(content.len(), match_idx + 80);

        let mut safe_start = start;
        while !content.is_char_boundary(safe_start) && safe_start > 0 {
            safe_start -= 1;
        }

        let mut safe_end = end;
        while !content.is_char_boundary(safe_end) && safe_end < content.len() {
            safe_end += 1;
        }

        let slice = &content[safe_start..safe_end];
        let cleaned = slice.replace(['\n', '\r'], " ").trim().to_string();

        let prefix = if safe_start > 0 { "..." } else { "" };
        let suffix = if safe_end < content.len() { "..." } else { "" };

        Some(format!("{}{}{}", prefix, cleaned, suffix))
    }

    pub fn extract_snippet_from_text(content: &str, query: &str) -> Option<String> {
        Self::extract_snippet_from_text_cached(content, &content.to_lowercase(), query)
    }
}

/// Bytes read when producing a snippet straight from a text file.
const SNIPPET_READ_LIMIT: u64 = 64 * 1024;

/// True when the extension is plain text, code or configuration.
///
/// Deliberately excludes PDF, Office/OpenDocument, eBook, RTF and image
/// formats: those need an extractor (or OCR) and must never be read as a text
/// file on the query path.
pub fn is_plain_text_extension(ext_opt: Option<&str>) -> bool {
    let ext = match ext_opt {
        Some(ext) => ext.to_ascii_lowercase(),
        None => return false,
    };

    static PLAIN_TEXT_EXTENSIONS: &[&str] = &[
        // Plain text / markup
        "txt",
        "md",
        "markdown",
        "rst",
        "tex",
        "log",
        "diff",
        "patch",
        // Code
        "rs",
        "ts",
        "tsx",
        "js",
        "jsx",
        "mjs",
        "cjs",
        "py",
        "go",
        "c",
        "cpp",
        "cc",
        "cxx",
        "h",
        "hpp",
        "java",
        "kt",
        "kts",
        "rb",
        "php",
        "swift",
        "r",
        "lua",
        "vim",
        "sh",
        "bash",
        "zsh",
        "fish",
        "sql",
        // Web
        "html",
        "htm",
        "css",
        "scss",
        "sass",
        "vue",
        "svelte",
        "astro",
        "graphql",
        "proto",
        // Config
        "json",
        "toml",
        "yaml",
        "yml",
        "xml",
        "ini",
        "cfg",
        "conf",
        "env",
        "dockerfile",
    ];

    PLAIN_TEXT_EXTENSIONS.contains(&ext.as_str())
}

/// Longest row snippet returned. Longer rows are cut and end with "...".
const ROW_SNIPPET_BYTES: usize = 200;

/// True for CSV and TSV files, which get row-aligned snippets.
pub fn is_delimited_extension(ext_opt: Option<&str>) -> bool {
    matches!(ext_opt.map(str::to_ascii_lowercase).as_deref(), Some("csv" | "tsv"))
}

/// Snippet for a delimited file: the first row that contains a query term.
/// Each row is one line, so the snippet never splits a row.
fn delimited_row_snippet<'a>(mut rows: impl Iterator<Item = &'a str>, query: &str) -> Option<String> {
    let query_lower = query.to_lowercase();
    let terms: Vec<&str> = query_lower.split_whitespace().collect();
    if terms.is_empty() {
        return None;
    }
    let row = rows.find(|row| {
        let lower = row.to_lowercase();
        terms.iter().any(|term| lower.contains(*term))
    })?;
    if row.len() <= ROW_SNIPPET_BYTES {
        return Some(row.to_string());
    }
    let clipped = crate::indexer::extract::delimited::clip_to_char_boundary(row, ROW_SNIPPET_BYTES);
    Some(format!("{clipped}..."))
}

/// Builds a snippet without extracting anything.
///
/// Plain text is read directly; everything else only reads the extraction
/// cache. See [`ContentIndex::get_snippet_fast`].
pub fn snippet_for_path(path: &Path, query: &str) -> Option<String> {
    if is_delimited_extension(path.extension().and_then(|ext| ext.to_str())) {
        let file = fs::File::open(path).ok()?;
        let truncated = file.metadata().ok()?.len() > SNIPPET_READ_LIMIT;
        let mut buffer = Vec::with_capacity(SNIPPET_READ_LIMIT as usize);
        file.take(SNIPPET_READ_LIMIT).read_to_end(&mut buffer).ok()?;
        let hint = crate::indexer::extract::delimited::hint_for_path(path);
        let rows = crate::indexer::extract::delimited::row_blocks(&buffer, truncated, hint);
        return delimited_row_snippet(rows.iter().map(String::as_str), query);
    }

    if is_plain_text_extension(path.extension().and_then(|ext| ext.to_str())) {
        let file = fs::File::open(path).ok()?;
        let mut buffer = Vec::with_capacity(SNIPPET_READ_LIMIT as usize);
        file.take(SNIPPET_READ_LIMIT)
            .read_to_end(&mut buffer)
            .ok()?;
        let text = String::from_utf8_lossy(&buffer);
        return ContentIndex::extract_snippet_from_text(&text, query);
    }

    let document = crate::indexer::extract::cached_document(path)?;
    if document.text.is_empty() {
        return None;
    }
    ContentIndex::extract_snippet_from_text(&document.text, query)
}

pub fn is_indexable_extension(ext_opt: Option<&str>) -> bool {
    let ext = match ext_opt {
        Some(e) => e.to_lowercase(),
        None => return false,
    };

    static TEXT_EXTENSIONS: &[&str] = &[
        // Plain text and code
        "txt",
        "md",
        "markdown",
        "rs",
        "ts",
        "tsx",
        "js",
        "jsx",
        "py",
        "go",
        "json",
        "toml",
        "yaml",
        "yml",
        "xml",
        "html",
        "htm",
        "css",
        "scss",
        "c",
        "cpp",
        "cc",
        "cxx",
        "h",
        "hpp",
        "java",
        "kt",
        "sh",
        "bash",
        "zsh",
        "fish",
        "env",
        "cfg",
        "ini",
        "conf",
        "log",
        "csv",
        "tsv",
        "parquet",
        "sql",
        "rb",
        "php",
        "swift",
        "r",
        "lua",
        "vim",
        "vue",
        "svelte",
        "astro",
        "dockerfile",
        "diff",
        "patch",
        "tex",
        "graphql",
        "proto",
        // Documents & Office (PR 5)
        "pdf",
        "doc",
        "docx",
        "ppt",
        "pptx",
        "xls",
        "xlsx",
        "epub",
        "odt",
        "ods",
        "odp",
        "rtf",
        "pages",
        "key",
        "numbers",
        "png",
        "jpg",
        "jpeg",
        "gif",
        "bmp",
        "webp",
        "tiff",
        "tif",
    ];
    TEXT_EXTENSIONS.contains(&ext.as_str())
}

pub fn is_indexable_file_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    matches!(
        lower.as_str(),
        "dockerfile"
            | "makefile"
            | "justfile"
            | "cargo.toml"
            | "package.json"
            | ".gitignore"
            | ".env"
            | ".editorconfig"
    )
}

pub fn read_text_content(path: &Path, max_bytes: u64) -> Option<String> {
    crate::indexer::extract::extract_document(path, max_bytes).map(|doc| doc.text)
}

/// [`read_text_content`] plus whether the read was incomplete and should be
/// retried (timeout, busy, or OCR that could not run yet).
pub fn read_text_content_checked(path: &Path, max_bytes: u64) -> (Option<String>, bool) {
    let (document, retry) = crate::indexer::extract::extract_document_checked(path, max_bytes);
    (document.map(|doc| doc.text), retry)
}

/// Defuses QueryParser metacharacters in user-supplied text.
///
/// Replacing them with spaces preserves the surrounding words while ensuring
/// raw `:`, quotes, `-`, boolean operators, etc. can never abort parsing.
pub fn sanitize_query_text(text: &str) -> String {
    text.replace(
        [
            ':', '^', '[', ']', '{', '}', '~', '*', '?', '\\', '(', ')', '"', '-', '+', '!', '&',
            '|', '/',
        ],
        " ",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "kensaku-content-test-{}-{}-{}",
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

    #[test]
    fn contains_near_detects_proximity_operator() {
        assert!(contains_near("mouse NEAR[5] button"));
        assert!(contains_near("a near[1] b"));
        assert!(!contains_near("mouse button"));
        assert!(!contains_near("neighborhood"));
        assert!(!contains_near("near"));
        assert!(!contains_near("near[]"));
        assert!(!contains_near("near[abc]"));
    }

    #[test]
    fn rewrite_near_builds_slop_phrases_and_drops_orphans() {
        assert_eq!(
            rewrite_near_in_group("mouse near[5] button"),
            "\"mouse button\"~5"
        );
        // Chains sum the operator distances into one phrase.
        assert_eq!(
            rewrite_near_in_group("mouse near[3] button near[2] click"),
            "\"mouse button click\"~5"
        );
        // Consecutive operators: only the last distance binds.
        assert_eq!(
            rewrite_near_in_group("mouse near[5] near[2] button"),
            "\"mouse button\"~2"
        );
        // Dangling/leading operators are dropped, never emitted as terms.
        assert_eq!(rewrite_near_in_group("mouse near[5]"), "mouse");
        assert_eq!(rewrite_near_in_group("near[5] mouse"), "mouse");
        // Chain slop is clamped to the documented maximum.
        assert_eq!(
            rewrite_near_in_group("a near[60] b near[60] c"),
            "\"a b c\"~100"
        );
        // Plain tokens still pass through sanitization.
        assert_eq!(rewrite_near_in_group("a near[1] c++"), "\"a c\"~1");
    }

    #[test]
    fn near_operator_limits_results_to_word_distance() {
        let temp = temp_index("near");
        let index = temp.index.as_ref().unwrap();
        add(
            index,
            "/tmp/kensaku/n/doc.txt",
            "doc.txt",
            "quick brown fox jumps over the lazy dog",
        );
        index.commit().unwrap();

        // "quick" -> "lazy" spans 6 words, i.e. slop 5 for a two-term phrase.
        let hits = index.search("quick near[5] lazy", 10, None);
        assert!(
            hits.iter().any(|m| m.path.ends_with("doc.txt")),
            "near[5] must match a 6-word gap"
        );
        assert!(
            index.search("quick near[1] lazy", 10, None).is_empty(),
            "near[1] must reject a 6-word gap"
        );
    }

    #[test]
    fn fold_maps_lookalikes_without_crossing_spaces() {
        assert_eq!(fold("0RDER"), "order");
        assert_eq!(fold("nutsmp"), "nutsmp");
        assert_eq!(fold("rnodern vvater clock"), "modem water dock");
        assert_eq!(fold("a rn"), "a m");
        assert_eq!(fold("email"), "emaii");
        assert_eq!(fold("internally"), "intemaiiy");
        assert_eq!(fold("5$8"), "ssb");
    }

    fn dir_size(path: &Path) -> u64 {
        let mut total = 0;
        let Ok(entries) = fs::read_dir(path) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
        total
    }

    #[test]
    fn records_folded_index_size_growth() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kensaku-content-size-{}-{}",
            std::process::id(),
            nanos
        ));
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&new_dir).unwrap();

        let mut old_schema = Schema::builder();
        old_schema.add_text_field("path", STRING | STORED);
        old_schema.add_text_field("filename", TEXT | STORED);
        old_schema.add_text_field("content", TEXT);
        old_schema.add_u64_field("modified", STORED | FAST);
        let old_index = Index::create_in_dir(&old_dir, old_schema.build()).unwrap();
        let mut old_writer = old_index.writer(50_000_000).unwrap();
        let old_schema = old_index.schema();
        let old_path = old_schema.get_field("path").unwrap();
        let old_name = old_schema.get_field("filename").unwrap();
        let old_content = old_schema.get_field("content").unwrap();
        let old_modified = old_schema.get_field("modified").unwrap();

        let mut new_schema_builder = Schema::builder();
        let new_path = new_schema_builder.add_text_field("path", STRING | STORED);
        let new_name = new_schema_builder.add_text_field("filename", TEXT | STORED);
        let new_content = new_schema_builder.add_text_field("content", TEXT);
        let folded_indexing = TextFieldIndexing::default()
            .set_tokenizer("trigram")
            .set_index_option(IndexRecordOption::WithFreqs);
        let new_folded = new_schema_builder.add_text_field(
            "content_folded",
            TextOptions::default().set_indexing_options(folded_indexing),
        );
        let new_raw = new_schema_builder.add_text_field("content_raw", STORED);
        let new_modified = new_schema_builder.add_u64_field("modified", STORED | FAST);
        let new_index = Index::create_in_dir(&new_dir, new_schema_builder.build()).unwrap();
        new_index
            .tokenizers()
            .register("trigram", NgramTokenizer::new(3, 3, false).unwrap());
        let mut new_writer = new_index.writer(50_000_000).unwrap();

        let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
                    order invoice screenshot receipt research notes";
        let text: String = (0..60).map(|_| body).collect::<Vec<_>>().join(" ");
        for i in 0..100 {
            let path = format!("/tmp/kensaku/size/doc-{i}.txt");
            old_writer
                .add_document(doc!(
                    old_path => path.clone(),
                    old_name => format!("doc-{i}.txt"),
                    old_content => text.clone(),
                    old_modified => i as u64,
                ))
                .unwrap();
            new_writer
                .add_document(doc!(
                    new_path => path,
                    new_name => format!("doc-{i}.txt"),
                    new_content => text.clone(),
                    new_folded => fold(&text),
                    new_raw => text.clone(),
                    new_modified => i as u64,
                ))
                .unwrap();
        }
        old_writer.commit().unwrap();
        new_writer.commit().unwrap();
        drop(old_writer);
        drop(new_writer);
        drop(old_index);
        drop(new_index);

        let old_bytes = dir_size(&old_dir);
        let new_bytes = dir_size(&new_dir);
        println!(
            "K5 folded index: 100 docs x {} B raw, old={} B new={} B growth={:.2}x",
            text.len(),
            old_bytes,
            new_bytes,
            new_bytes as f64 / old_bytes.max(1) as f64
        );
        assert!(new_bytes > old_bytes);
        let _ = fs::remove_dir_all(&root);
    }

    fn add(index: &ContentIndex, path: &str, filename: &str, content: &str) {
        index
            .add_or_update(
                Path::new(path),
                filename,
                content,
                0,
                0,
                Path::new(path).extension().and_then(|ext| ext.to_str()),
            )
            .expect("add document");
    }

    #[test]
    #[ignore = "reports folded p50/p95; run with --ignored --nocapture"]
    fn folded_query_latency() {
        let temp = temp_index("folded-latency");
        let index = temp.index.as_ref().unwrap();
        for i in 0..500 {
            let body = format!(
                "invoice invoice number {i} customer order receipt screenshot \
                 alpha beta gamma delta epsilon zeta eta theta iota kappa"
            );
            add(
                index,
                &format!("/tmp/kensaku/latency/doc-{i}.txt"),
                &format!("doc-{i}.txt"),
                &body,
            );
        }
        index.commit().unwrap();

        let mut samples = Vec::with_capacity(200);
        for _ in 0..200 {
            let start = std::time::Instant::now();
            let hits = index.search_folded("invoice", 50, None);
            std::hint::black_box(hits);
            samples.push(start.elapsed().as_micros() as u64);
        }
        samples.sort_unstable();
        println!(
            "K5 folded query latency over 500 docs: p50={}µs p95={}µs",
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100]
        );
    }

    #[test]
    fn verify_folded_accepts_lookalike_mappings() {
        assert!(verify_folded("the clerk signed it", "the derk signed it"));
        assert!(verify_folded("vvindow", "window"));
        assert!(verify_folded("rnment", "mment"));
        assert!(verify_folded("5tr", "str"));
        assert!(verify_folded("8ig", "big"));
        assert!(verify_folded("f1le", "fiie"));
        assert!(verify_folded("0rder", "order"));
        assert!(!verify_folded("internally consistent", "email"));
    }

    #[test]
    fn folded_search_handles_cjk_and_skips_short_terms() {
        let temp = temp_index("folded-cjk");
        let index = temp.index.as_ref().unwrap();
        add(
            index,
            "/tmp/kensaku/cjk/paper.txt",
            "paper.txt",
            "東京大学の研究成果",
        );
        index.commit().unwrap();

        let hit = index.search_folded("東京大", 10, None);
        assert_eq!(hit.len(), 1);
        // Terms under three characters are left to the exact/bounded fallback.
        assert!(index.search_folded("東京", 10, None).is_empty());
    }

    #[test]
    fn old_schema_is_backed_up_and_rebuilt() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kensaku-content-migrate-{}-{}",
            std::process::id(),
            nanos
        ));
        let index_dir = root.join("index");
        fs::create_dir_all(&index_dir).unwrap();

        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("path", STRING | STORED);
        schema_builder.add_text_field("filename", TEXT | STORED);
        schema_builder.add_text_field("content", TEXT);
        schema_builder.add_u64_field("modified", STORED | FAST);
        let old_index =
            Index::create_in_dir(&index_dir, schema_builder.build()).expect("old index");
        drop(old_index);

        let index = ContentIndex::new(&index_dir).expect("migrate index");
        assert!(index.rebuilt);
        assert!(index.content_folded_field.is_some());
        assert!(index.content_raw_field.is_some());
        assert!(
            fs::read_dir(&root).unwrap().flatten().any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("index.legacy-")),
            "old derived index should be preserved beside the new one"
        );

        index
            .add_or_update(
                Path::new("/tmp/kensaku/migrated/note.txt"),
                "note.txt",
                "Visit donutsmp.net",
                0,
                0,
                Some("txt"),
            )
            .unwrap();
        index.commit().unwrap();
        assert_eq!(index.search_folded("nutsmp", 10, None).len(), 1);

        drop(index);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn schema_v3_index_rebuilds_once_then_stays_put() {
        // The upgrade this release makes: an index written at schema 3 must be
        // rebuilt into the positionless trigram format, and must not be rebuilt
        // again on the next launch. A rebuild on every launch is the exact
        // failure mode reported earlier, so it is pinned here.
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kensaku-content-v4-{}-{}",
            std::process::id(),
            nanos
        ));
        let index_dir = root.join("index");

        let fresh = ContentIndex::new(&index_dir).expect("fresh index");
        drop(fresh);

        // Pretend this directory was written by the previous release.
        fs::write(index_dir.join(SCHEMA_MARKER_FILE), "3").unwrap();

        let migrated = ContentIndex::new(&index_dir).expect("migrate v3 index");
        assert!(migrated.rebuilt, "a schema 3 index must be rebuilt");
        assert_eq!(
            migrated.rebuild_reason.as_deref(),
            Some("trigram posting format change (schema v3 -> v4, #129)")
        );
        migrated
            .add_or_update(
                Path::new("/tmp/kensaku/v4/note.txt"),
                "note.txt",
                "Visit donutsmp.net",
                0,
                0,
                Some("txt"),
            )
            .unwrap();
        migrated.commit().unwrap();
        assert_eq!(
            migrated.search_folded("nutsmp", 10, None).len(),
            1,
            "folded search must keep working on the rebuilt index"
        );
        drop(migrated);

        let reopened = ContentIndex::new(&index_dir).expect("reopen v4 index");
        assert!(!reopened.rebuilt, "a schema 4 index must not rebuild on launch");
        drop(reopened);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folded_search_finds_trigram_substrings_and_lookalikes() {
        let temp = temp_index("folded");
        let index = temp.index.as_ref().unwrap();
        add(
            index,
            "/tmp/kensaku/f/donuts.txt",
            "donuts.txt",
            "Visit donutsmp.net for details",
        );
        add(
            index,
            "/tmp/kensaku/f/order.txt",
            "order.txt",
            "ORDER-12345 shipped",
        );
        add(
            index,
            "/tmp/kensaku/f/unrelated.txt",
            "unrelated.txt",
            "nothing relevant here",
        );
        index.commit().unwrap();

        // Exact BM25 cannot match an interior substring of a token.
        assert!(index.search("nutsmp", 10, None).is_empty());
        let substring = index.search_folded("nutsmp", 10, None);
        assert_eq!(substring.len(), 1);
        assert!(substring[0].path.ends_with("donuts.txt"));

        // 0/o lookalike is folded; verification allows one misread.
        assert!(index.search("0RDER", 10, None).is_empty());
        let lookalike = index.search_folded("0RDER", 10, None);
        assert_eq!(lookalike.len(), 1);
        assert!(lookalike[0].path.ends_with("order.txt"));
    }

    #[test]
    fn folded_search_rejects_broad_ngram_false_positives() {
        let temp = temp_index("folded-reject");
        let index = temp.index.as_ref().unwrap();
        add(
            index,
            "/tmp/kensaku/f/clock.txt",
            "clock.txt",
            "the clock tower",
        );
        add(
            index,
            "/tmp/kensaku/f/email.txt",
            "email.txt",
            "internally consistent",
        );
        index.commit().unwrap();

        // d/cl folding makes "clock" fold to "dock", so the folded tier
        // accepts it. "email" still has no folded match in "internally".
        let dock = index.search_folded("dock", 10, None);
        assert_eq!(dock.len(), 1);
        assert!(dock[0].path.ends_with("clock.txt"));
        assert!(index.search_folded("email", 10, None).is_empty());
        assert!(verify_folded("the order shipped", "0RDER"));
        assert!(verify_folded("donutsmp.net", "nutsmp"));
        assert!(!verify_folded("internally consistent", "email"));
        assert!(verify_folded("the clock tower", "dock"));
    }

    #[test]
    fn field_scoped_groups_only_hit_their_field() {
        let temp = temp_index("groups");
        let index = temp.index.as_ref().unwrap();
        add(
            index,
            "/tmp/kensaku/a/alpha.rs",
            "alpha.rs",
            "unique content zebra",
        );
        add(
            index,
            "/tmp/kensaku/a/beta.rs",
            "beta.rs",
            "unique content alpha",
        );
        index.commit().unwrap();

        let by_name = index.search_groups(&[(FieldKind::Name, "alpha")], 10, None);
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].filename, "alpha.rs");

        let by_content = index.search_groups(&[(FieldKind::Content, "alpha")], 10, None);
        assert_eq!(by_content.len(), 1);
        assert_eq!(by_content[0].filename, "beta.rs");

        let any = index.search_groups(&[(FieldKind::Any, "alpha")], 10, None);
        assert_eq!(any.len(), 2);
    }

    #[test]
    fn regex_clauses_match_both_fields_and_invalid_patterns_hint() {
        let temp = temp_index("regex");
        let index = temp.index.as_ref().unwrap();
        add(
            index,
            "/tmp/kensaku/r/one.rs",
            "one.rs",
            "the zebra lives here",
        );
        add(index, "/tmp/kensaku/r/two.rs", "two.rs", "nothing to see");
        index.commit().unwrap();

        let by_content = index.search_groups_with_regex(&[], &[".*zebra.*"], 10, None);
        assert!(by_content.hints.is_empty());
        assert_eq!(by_content.matches.len(), 1);
        assert_eq!(by_content.matches[0].filename, "one.rs");

        let by_filename = index.search_groups_with_regex(&[], &[".*two.*"], 10, None);
        assert_eq!(by_filename.matches.len(), 1);
        assert_eq!(by_filename.matches[0].filename, "two.rs");

        let invalid = index.search_groups_with_regex(&[], &["[unterminated"], 10, None);
        assert!(invalid.matches.is_empty());
        assert_eq!(invalid.hints.len(), 1);
        assert!(invalid.hints[0].contains("Invalid regex"));

        let too_long = "a".repeat(MAX_REGEX_PATTERN_LEN + 1);
        let capped = index.search_groups_with_regex(&[], &[too_long.as_str()], 10, None);
        assert!(capped.matches.is_empty());
        assert!(capped.hints.iter().any(|h| h.contains("too long")));
    }

    #[test]
    fn read_only_handle_sees_committed_documents_while_writer_is_open() {
        let temp = temp_index("read-only");
        let writer_index = temp.index.as_ref().unwrap();
        add(
            writer_index,
            "/tmp/kensaku/ro/hello.txt",
            "hello.txt",
            "read only needle content",
        );
        writer_index.commit().unwrap();

        // The writer (and its exclusive Tantivy lock) is still alive here.
        let reader = ContentIndex::open_read_only(&temp.dir).expect("open read-only");
        assert!(!reader.is_writable());
        assert!(reader.num_docs() >= 1);

        let matches = reader.search_groups(&[(FieldKind::Content, "needle")], 10, None);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].filename, "hello.txt");

        // Mutations are rejected instead of touching the shared index.
        let err = reader
            .add_or_update(
                Path::new("/tmp/kensaku/ro/other.txt"),
                "other.txt",
                "x",
                0,
                0,
                Some("txt"),
            )
            .unwrap_err();
        assert_eq!(err, "index is read-only");
        assert_eq!(reader.commit().unwrap_err(), "index is read-only");
        assert_eq!(
            reader.purge_stale(&HashSet::new(), &[]).unwrap_err(),
            "index is read-only"
        );
    }

    #[test]
    fn fast_snippet_reads_text_and_never_extracts_documents() {
        let temp = temp_index("fast-snippet");
        let index = temp.index.as_ref().unwrap();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let text_path = std::env::temp_dir().join(format!(
            "kensaku-fast-snippet-{}-{}.txt",
            std::process::id(),
            nanos
        ));
        fs::write(&text_path, "alpha line\nbeta needle line\nomega line\n").unwrap();
        let snippet = index
            .get_snippet_fast(&text_path, "needle")
            .expect("text snippet");
        assert!(snippet.contains("needle"), "got: {snippet}");
        let _ = fs::remove_file(&text_path);

        // A document format with a cold extraction cache returns `None`
        // immediately instead of running the PDF extractor at query time.
        let pdf_path = std::env::temp_dir().join(format!(
            "kensaku-fast-snippet-{}-{}.pdf",
            std::process::id(),
            nanos
        ));
        fs::write(&pdf_path, b"%PDF-1.4\nnot a real document\n").unwrap();
        let started = std::time::Instant::now();
        assert!(index.get_snippet_fast(&pdf_path, "needle").is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "cold document snippet took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_file(&pdf_path);
    }

    #[test]
    fn csv_snippet_returns_the_matching_row() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kensaku-csv-snippet-{}-{nanos}.csv",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "name,city,note\nAda,London,first programmer\nGrace,New York,admiral\n",
        )
        .unwrap();
        let snippet = snippet_for_path(&path, "london");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            snippet.as_deref(),
            Some("name = \"Ada\", city = \"London\", note = \"first programmer\"")
        );
    }

    #[test]
    fn plain_text_extensions_exclude_documents_and_images() {
        assert!(is_plain_text_extension(Some("txt")));
        assert!(is_plain_text_extension(Some("RS")));
        assert!(is_plain_text_extension(Some("json")));
        for excluded in [
            "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "epub", "odt", "ods", "odp", "rtf",
            "png", "jpg", "jpeg", "gif", "webp", "tiff", "heic",
        ] {
            assert!(
                !is_plain_text_extension(Some(excluded)),
                "{excluded} must not be treated as plain text"
            );
        }
        assert!(!is_plain_text_extension(None));
    }

    #[test]
    fn open_read_only_requires_an_existing_index() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-content-missing-{}-{}",
            std::process::id(),
            nanos
        ));
        let err = match ContentIndex::open_read_only(&dir) {
            Ok(_) => panic!("read-only open should fail without an index"),
            Err(err) => err,
        };
        assert!(err.contains("No content index"), "unexpected error: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cjk_search_exact_and_substring() {
        let temp = temp_index("cjk-search");
        let index = temp.index.as_ref().unwrap();

        add(
            index,
            "/tmp/kensaku/cjk/report.pdf",
            "最終報告書.pdf",
            "東京大学の研究発表と2026年度予算",
        );
        add(
            index,
            "/tmp/kensaku/cjk/weather.txt",
            "weather.txt",
            "東京都の天気予報は晴れです",
        );
        add(
            index,
            "/tmp/kensaku/cjk/readme.md",
            "README最終.md",
            "Project documentation in English",
        );
        add(
            index,
            "/tmp/kensaku/cjk/comp.ts",
            "组件.ts",
            "export const button = 1;",
        );
        index.commit().unwrap();

        // CJK filename exact and substring
        let hits = index.search("最終報告書", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "最終報告書.pdf");

        let hits = index.search("報告", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "最終報告書.pdf");

        // Single-character CJK query
        let hits = index.search("最", 10, None);
        assert_eq!(hits.len(), 2);
        let names: Vec<&str> = hits.iter().map(|h| h.filename.as_str()).collect();
        assert!(names.contains(&"最終報告書.pdf"));
        assert!(names.contains(&"README最終.md"));

        // CJK content word and substring
        let hits = index.search("東京", 10, None);
        assert_eq!(hits.len(), 2);

        let hits = index.search("予報", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "weather.txt");

        // Mixed-script queries
        let hits = index.search("README最終", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "README最終.md");

        let hits = index.search("README", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "README最終.md");

        let hits = index.search("组件", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "组件.ts");

        // Scoped searches
        let hits = index.search_groups(&[(FieldKind::Name, "報告")], 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "最終報告書.pdf");

        let hits = index.search_groups(&[(FieldKind::Content, "報告")], 10, None);
        assert!(hits.is_empty());

        let hits = index.search_groups(&[(FieldKind::Content, "晴れ")], 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "weather.txt");
    }

    #[test]
    fn schema_v2_without_pushdown_fields_triggers_rebuild() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kensaku-pushdown-migrate-{}-{}",
            std::process::id(),
            nanos
        ));
        let index_dir = root.join("index");
        fs::create_dir_all(&index_dir).unwrap();

        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("path", STRING | STORED);
        schema_builder.add_text_field("filename", TEXT | STORED);
        schema_builder.add_text_field("content", TEXT);
        let folded_indexing = TextFieldIndexing::default()
            .set_tokenizer("trigram")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        schema_builder.add_text_field(
            "content_folded",
            TextOptions::default().set_indexing_options(folded_indexing),
        );
        schema_builder.add_text_field("content_raw", STORED);
        schema_builder.add_u64_field("modified", STORED | FAST);
        let v2_index = Index::create_in_dir(&index_dir, schema_builder.build()).unwrap();
        drop(v2_index);
        fs::write(index_dir.join(SCHEMA_MARKER_FILE), "2").unwrap();

        let migrated = ContentIndex::new(&index_dir).expect("migrate to v3");
        assert!(migrated.rebuilt);
        assert_eq!(
            migrated.rebuild_reason.as_deref(),
            Some("K12 filter pushdown migration (schema v2 -> v3)")
        );
        let marker = fs::read_to_string(index_dir.join(SCHEMA_MARKER_FILE)).unwrap();
        assert_eq!(marker.trim(), CONTENT_SCHEMA_VERSION.to_string());
        assert!(migrated.ext_field.is_some());
        assert!(migrated.size_field.is_some());
        drop(migrated);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn schema_v1_without_marker_triggers_rebuild() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kensaku-cjk-migrate-{}-{}",
            std::process::id(),
            nanos
        ));
        let index_dir = root.join("index");
        fs::create_dir_all(&index_dir).unwrap();

        // Create a legacy K5 index with content_folded and content_raw, but WITHOUT content.schema
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("path", STRING | STORED);
        schema_builder.add_text_field("filename", TEXT | STORED);
        schema_builder.add_text_field("content", TEXT);
        let folded_indexing = TextFieldIndexing::default()
            .set_tokenizer("trigram")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        schema_builder.add_text_field(
            "content_folded",
            TextOptions::default().set_indexing_options(folded_indexing),
        );
        schema_builder.add_text_field("content_raw", STORED);
        schema_builder.add_u64_field("modified", STORED | FAST);
        let v1_index = Index::create_in_dir(&index_dir, schema_builder.build()).unwrap();
        v1_index
            .tokenizers()
            .register("trigram", NgramTokenizer::new(3, 3, false).unwrap());
        drop(v1_index);

        // First open: missing content.schema triggers rebuild
        let migrated = ContentIndex::new(&index_dir).expect("migrate to v2");
        assert!(migrated.rebuilt);
        assert_eq!(
            migrated.rebuild_reason.as_deref(),
            Some("K15 CJK tokenizer migration (schema v1 -> v2)")
        );
        assert!(index_dir.join(SCHEMA_MARKER_FILE).exists());
        let marker = fs::read_to_string(index_dir.join(SCHEMA_MARKER_FILE)).unwrap();
        assert_eq!(marker.trim(), CONTENT_SCHEMA_VERSION.to_string());
        drop(migrated);
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Second open: marker exists, no rebuild triggered
        let second_open = ContentIndex::new(&index_dir).expect("open already-migrated index");
        assert!(!second_open.rebuilt);
        assert!(second_open.rebuild_reason.is_none());
        drop(second_open);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cjk_index_size_growth_is_measured() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("kensaku-cjk-size-{}-{}", std::process::id(), nanos));
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&new_dir).unwrap();

        // Old schema: SimpleTokenizer on filename and content
        let mut old_builder = Schema::builder();
        let old_path = old_builder.add_text_field("path", STRING | STORED);
        let old_filename = old_builder.add_text_field("filename", TEXT | STORED);
        let old_content = old_builder.add_text_field("content", TEXT);
        let old_modified = old_builder.add_u64_field("modified", STORED | FAST);
        let old_index = Index::create_in_dir(&old_dir, old_builder.build()).unwrap();
        let mut old_writer = old_index.writer(16_000_000).unwrap();

        // New schema: ContentIndex with kensaku tokenizer
        let new_index = ContentIndex::new(&new_dir).unwrap();

        let cjk_sample_titles = [
            "最終報告書.pdf",
            "東京都天気予報.txt",
            "研究成果発表資料.docx",
            "年度予算概算要求書.xlsx",
            "システム設計仕様書.md",
        ];
        let cjk_sample_body = "東京大学情報理工学系研究科の研究チームは2026年度予算案に基づき人工知能検索エンジンの評価を実施した。日本語および中国語韓国語の形態素解析と文字分割の正確性を検証する。";

        for i in 0..50 {
            let title = cjk_sample_titles[i % cjk_sample_titles.len()];
            let path_str = format!("/cjk/doc_{i}/{title}");
            let path = PathBuf::from(&path_str);
            old_writer
                .add_document(doc!(
                    old_path => path_str,
                    old_filename => title.to_string(),
                    old_content => cjk_sample_body.to_string(),
                    old_modified => i as u64,
                ))
                .unwrap();

            new_index
                .add_or_update(&path, title, cjk_sample_body, i as u64, 0, Some("txt"))
                .unwrap();
        }
        old_writer.commit().unwrap();
        new_index.commit().unwrap();

        drop(old_writer);
        drop(old_index);
        drop(new_index);

        let old_bytes = dir_size(&old_dir);
        let new_bytes = dir_size(&new_dir);
        let ratio = new_bytes as f64 / old_bytes.max(1) as f64;
        println!(
            "K15 CJK index size: 50 docs x ~100 CJK chars: simple={} B, kensaku(cjk+folded)={} B, ratio={:.2}x",
            old_bytes, new_bytes, ratio
        );
        assert!(new_bytes > 0);
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn ext_and_size_filters_are_pushed_into_the_query() {
        let temp = temp_index("pushdown");
        let index = temp.index.as_ref().unwrap();
        for (name, size) in [
            ("small.pdf", 10u64),
            ("big.pdf", 5_000_000),
            ("note.md", 10),
        ] {
            let path = format!("/tmp/kensaku/pushdown/{}", name);
            index
                .add_or_update(
                    Path::new(&path),
                    name,
                    "needle invoice",
                    0,
                    size,
                    Path::new(&path).extension().and_then(|ext| ext.to_str()),
                )
                .unwrap();
        }
        index.commit().unwrap();

        let all = index.search_groups_filtered(
            &[(FieldKind::Content, "needle")],
            &[],
            10,
            None,
            &ContentFilters::default(),
        );
        assert_eq!(all.matches.len(), 3);

        let pdf = ContentFilters {
            ext: Some("pdf".to_string()),
            size: None,
        };
        assert_eq!(
            index
                .search_groups_filtered(&[(FieldKind::Content, "needle")], &[], 10, None, &pdf,)
                .matches
                .len(),
            2
        );

        let small_pdf = ContentFilters {
            ext: Some("PDF".to_string()),
            size: Some(RangeOp::Lt(100)),
        };
        let hits = index.search_groups_filtered(
            &[(FieldKind::Content, "needle")],
            &[],
            10,
            None,
            &small_pdf,
        );
        assert_eq!(hits.matches.len(), 1);
        assert!(hits.matches[0].path.ends_with("small.pdf"));

        let folded = index.search_folded_filtered(
            "needle",
            10,
            None,
            &ContentFilters {
                ext: Some("pdf".to_string()),
                size: None,
            },
        );
        assert_eq!(folded.len(), 2, "folded tier must honor pushdown too");
    }
}
