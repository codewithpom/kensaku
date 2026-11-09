//! `find_symbol`: code-symbol lookup backed by tree-sitter.
//!
//! The tool takes a symbol name, pre-filters candidate files through the
//! normal content index (the model never does retrieval itself, and neither
//! does this tool: the index decides which files are worth parsing), then
//! parses each candidate with the right grammar and returns matching
//! **definitions** with path, kind, line range and a one-line signature.
//!
//! Supported languages: Rust, Python, JavaScript/JSX, TypeScript/TSX. The
//! parser set is deliberately small; unknown extensions are skipped, and a
//! parse failure skips the file rather than failing the call.

use crate::agent::AgentSession;
use serde_json::{Value, json};

/// Files parsed per call, hard cap. Candidates come from the content index,
/// so a broad name still stays bounded.
const MAX_CANDIDATE_FILES: usize = 200;
/// Files larger than this are skipped (they are generated or vendored).
const MAX_PARSE_BYTES: u64 = 512 * 1024;
const DEFAULT_PAGE_SIZE: usize = 10;
const MAX_PAGE_SIZE: usize = 50;

#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub path: String,
    pub name: String,
    pub kind: &'static str,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: String,
}

/// Languages understood by the extractor, keyed by extension.
fn language_for_extension(ext: &str) -> Option<Language> {
    match ext {
        "rs" => Some(Language::Rust),
        "py" | "pyi" => Some(Language::Python),
        "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
        "ts" | "tsx" | "mts" | "cts" => Some(Language::TypeScript),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
}

impl Language {
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(Self::Rust),
            "python" | "py" => Some(Self::Python),
            "javascript" | "js" => Some(Self::JavaScript),
            "typescript" | "ts" => Some(Self::TypeScript),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
        }
    }

    fn extensions(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::Python => &["py", "pyi"],
            Self::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Self::TypeScript => &["ts", "tsx", "mts", "cts"],
        }
    }
}

/// Extracts definitions from one source file. Returns an empty vec on parse
/// failure; the caller decides whether that is worth reporting.
pub fn extract_symbols(path: &std::path::Path) -> Vec<SymbolHit> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let Some(language) = language_for_extension(&extension) else {
        return Vec::new();
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if !metadata.is_file() || metadata.len() > MAX_PARSE_BYTES {
        return Vec::new();
    }
    let Ok(source) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_source(&source, language)
        .into_iter()
        .map(|hit| SymbolHit {
            path: path.to_string_lossy().into_owned(),
            ..hit
        })
        .collect()
}

fn parse_source(source: &str, language: Language) -> Vec<SymbolHit> {
    let mut parser = tree_sitter::Parser::new();
    let set = |parser: &mut tree_sitter::Parser, language: tree_sitter::Language| {
        parser
            .set_language(&language)
            .ok()
            .and_then(|_| parser.parse(source, None))
    };
    let parsed = match language {
        Language::Rust => set(&mut parser, tree_sitter_rust::LANGUAGE.into()),
        Language::Python => set(&mut parser, tree_sitter_python::LANGUAGE.into()),
        Language::JavaScript => set(&mut parser, tree_sitter_javascript::LANGUAGE.into()),
        Language::TypeScript => set(
            &mut parser,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
    };
    let Some(tree) = parsed else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    walk_definitions(tree.root_node(), source, language, &mut hits);
    hits
}

/// Depth-first walk collecting definition nodes. Nested definitions
/// (methods inside impls, functions inside classes) are included.
fn walk_definitions(
    node: tree_sitter::Node<'_>,
    source: &str,
    language: Language,
    hits: &mut Vec<SymbolHit>,
) {
    if let Some((name, kind)) = definition_of(&node, source, language) {
        let start = node.start_position();
        let end = node.end_position();
        let signature = node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim_end()
            .to_string();
        hits.push(SymbolHit {
            path: String::new(),
            name,
            kind,
            line_start: start.row + 1,
            line_end: end.row + 1,
            signature,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_definitions(child, source, language, hits);
    }
}

/// Maps a definition node to `(name, kind)` for the language.
fn definition_of(
    node: &tree_sitter::Node<'_>,
    source: &str,
    language: Language,
) -> Option<(String, &'static str)> {
    let kind: &'static str = match (language, node.kind()) {
        (Language::Rust, "function_item") => "function",
        (Language::Rust, "function_signature_item") => "function",
        (Language::Rust, "struct_item") => "struct",
        (Language::Rust, "enum_item") => "enum",
        (Language::Rust, "trait_item") => "trait",
        (Language::Rust, "type_item") => "type",
        (Language::Rust, "const_item") => "const",
        (Language::Rust, "static_item") => "static",
        (Language::Rust, "mod_item") => "module",
        (Language::Rust, "macro_definition") => "macro",
        (Language::Python, "function_definition") => "function",
        (Language::Python, "class_definition") => "class",
        (Language::JavaScript, "function_declaration") => "function",
        (Language::JavaScript, "generator_function_declaration") => "function",
        (Language::JavaScript, "class_declaration") => "class",
        (Language::JavaScript, "method_definition") => "method",
        (Language::TypeScript, "function_declaration") => "function",
        (Language::TypeScript, "generator_function_declaration") => "function",
        (Language::TypeScript, "class_declaration") => "class",
        (Language::TypeScript, "method_definition") => "method",
        (Language::TypeScript, "abstract_class_declaration") => "class",
        (Language::TypeScript, "interface_declaration") => "interface",
        (Language::TypeScript, "type_alias_declaration") => "type",
        (Language::TypeScript, "enum_declaration") => "enum",
        (Language::TypeScript, "internal_module") => "module",
        (Language::TypeScript, "module") => "module",
        _ => return None,
    };

    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_string();
    Some((name, kind))
}

/// Runs the `find_symbol` tool against a session and returns the MCP payload.
pub fn tool_find_symbol(session: &AgentSession<'_>, args: &Value) -> Result<Value, (String, bool)> {
    let Some(query) = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
    else {
        return Err(("Invalid params: 'query' is required".to_string(), true));
    };
    if query.chars().count() > 100 {
        return Err((
            "Invalid params: 'query' must be at most 100 characters".to_string(),
            true,
        ));
    }
    let language = match args.get("language").and_then(Value::as_str) {
        None | Some("") => None,
        Some(name) => match Language::parse(name) {
            Some(language) => Some(language),
            None => {
                return Err((
                    "Invalid params: 'language' must be rust, python, javascript or typescript"
                        .to_string(),
                    true,
                ));
            }
        },
    };
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let page_size = args
        .get("page_size")
        .and_then(Value::as_u64)
        .map(|size| size.clamp(1, MAX_PAGE_SIZE as u64) as usize)
        .unwrap_or(DEFAULT_PAGE_SIZE);

    // Scope enforcement matches the other read tools, and the policy fails
    // closed instead of silently becoming "no roots" on a config error.
    let policy = session.policy().map_err(|error| (error, false))?;
    if let Some(path) = path {
        if !policy.read_allowed(path) {
            return Err((
                "Invalid params: path is outside the configured agent scope".to_string(),
                true,
            ));
        }
        if !session
            .contains_path(path)
            .map_err(|error| (error, false))?
        {
            return Err((
                "Invalid params: path is not part of the index".to_string(),
                true,
            ));
        }
    }

    // Candidate files: an explicit path, else content-index prefilter.
    let candidate_paths: Vec<String> = if let Some(path) = path {
        vec![path.to_string()]
    } else {
        // Without a path this used to run `content:"query"` over the whole
        // index, ignoring the configured agent scope. Compose the same scoped
        // query as every other read tool, and escape the symbol name so it
        // cannot inject query syntax.
        let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
        let scope_paths = policy.query_scopes(None).map_err(|error| (error, true))?;
        let search_query = super::compose_query_scoped(
            &format!("content:\"{escaped}\""),
            &scope_paths,
            None,
            None,
            false,
        );
        match session.search_page(&search_query, MAX_CANDIDATE_FILES, 0) {
            Ok(response) => response
                .results
                .into_iter()
                .map(|result| result.path)
                .filter(|path| policy.path_allowed(path))
                .collect(),
            Err(error) => return Err((error, false)),
        }
    };

    // Parse and collect.
    let mut hits: Vec<SymbolHit> = Vec::new();
    let mut files_parsed = 0usize;
    let needle = query.to_lowercase();
    for candidate in candidate_paths.iter().take(MAX_CANDIDATE_FILES) {
        if !policy.path_allowed(candidate) {
            continue;
        }
        let candidate_path = std::path::Path::new(candidate);
        // Cheap pre-check: the file must mention the symbol at all.
        let extension = candidate_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        let Some(extension) = extension else { continue };
        if let Some(language) = language
            && !language.extensions().contains(&extension.as_str())
        {
            continue;
        }

        if let Ok(source) = std::fs::read_to_string(candidate_path)
            && !source.to_lowercase().contains(&needle)
        {
            continue;
        }
        let extracted = extract_symbols(candidate_path);
        files_parsed += 1;
        for hit in extracted {
            if hit.name.to_lowercase().contains(&needle) {
                hits.push(hit);
            }
        }
        if files_parsed >= MAX_CANDIDATE_FILES {
            break;
        }
    }

    // Exact matches first, then prefix, then substring; ties by path then line.
    hits.sort_by(|a, b| {
        let rank = |hit: &SymbolHit| -> u8 {
            if hit.name == query {
                0
            } else if hit.name.to_lowercase().starts_with(&needle) {
                1
            } else {
                2
            }
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line_start.cmp(&b.line_start))
    });

    let total = hits.len();
    let returned = hits.iter().take(page_size);
    let symbols: Vec<Value> = returned
        .map(|hit| {
            json!({
                "path": hit.path,
                "name": hit.name,
                "kind": hit.kind,
                "line_start": hit.line_start,
                "line_end": hit.line_end,
                "signature": hit.signature,
            })
        })
        .collect();

    Ok(json!({
        "query": query,
        "language": language.map(|l| l.name()),
        "symbols": symbols,
        "returned": symbols.len(),
        "total": total,
        "files_parsed": files_parsed,
        "has_more": total > symbols.len(),
        "as_of": crate::agent::now_unix_secs(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SAMPLE: &str = r#"
pub mod inner {
    pub fn visible() {}
    struct Hidden;
    impl Hidden {
        fn from_impl(&self) {}
    }
}
trait Doable { fn act(&self); }
enum Choice { A, B }
"#;

    #[test]
    fn rust_definitions_are_extracted_with_lines() {
        let mut hits = parse_source(RUST_SAMPLE, Language::Rust);
        hits.sort_by(|a, b| {
            a.line_start
                .cmp(&b.line_start)
                .then_with(|| a.name.cmp(&b.name))
        });
        let names: Vec<&str> = hits.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "inner",
                "visible",
                "Hidden",
                "from_impl",
                "Doable",
                "act",
                "Choice"
            ]
        );
        let visible = hits.iter().find(|hit| hit.name == "visible").unwrap();
        assert_eq!(visible.kind, "function");
        assert!(visible.signature.contains("fn visible"));
    }

    #[test]
    fn python_definitions_are_extracted() {
        let source =
            "class thing:\n    def method(self):\n        pass\n\ndef top_level():\n    pass\n";
        let hits = parse_source(source, Language::Python);
        let names: Vec<&str> = hits.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(names, vec!["thing", "method", "top_level"]);
        assert!(hits.iter().any(|hit| hit.kind == "class"));
    }

    #[test]
    fn typescript_definitions_are_extracted() {
        let source = "interface Shape { area(): number }\ntype Alias = string;\nclass Circle implements Shape { area() { return 0 } }\nfunction make(): Circle { return new Circle() }\n";
        let hits = parse_source(source, Language::TypeScript);
        let names: Vec<&str> = hits.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(names, vec!["Shape", "Alias", "Circle", "area", "make"]);
    }

    #[test]
    fn javascript_definitions_are_extracted() {
        let source = "function alpha() {}\nclass beta { gamma() {} }\n";
        let hits = parse_source(source, Language::JavaScript);
        let names: Vec<&str> = hits.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn unsupported_extensions_return_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "kensaku-sym-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.txt");
        std::fs::write(&file, "fn not_code() {}").unwrap();
        assert!(extract_symbols(&file).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_rejects_missing_query_and_bad_language() {
        let (message, invalid) = tool_find_symbol(&AgentSession::None, &json!({})).unwrap_err();
        assert!(invalid && message.contains("'query' is required"));

        let (message, invalid) = tool_find_symbol(
            &AgentSession::None,
            &json!({"query": "x", "language": "cobol"}),
        )
        .unwrap_err();
        assert!(invalid && message.contains("language"));
    }

    #[test]
    fn tool_reports_no_index_as_execution_error() {
        let (message, invalid) =
            tool_find_symbol(&AgentSession::None, &json!({"query": "main"})).unwrap_err();
        assert!(!invalid);
        assert!(message.contains("No Kensaku index"));
    }
}
