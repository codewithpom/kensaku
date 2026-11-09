//! Model Context Protocol server over stdio.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout. **stdout is
//! reserved for protocol messages**; diagnostics go to stderr.
//!
//! When `kensaku-daemon` is running the session forwards searches and state
//! reads to it and never opens SQLite, Tantivy or the embedding table here.
//! Without a daemon the read-only local index is used as a fallback. The only
//! write-capable tool (`reindex`) is disabled unless `mcp_allow_reindex` is
//! explicitly enabled in config.

use super::{AgentSession, HeadlessIndex, compose_query_scoped, now_unix_secs, truncate_chars};
use crate::config::KensakuConfig;
use crate::indexer::content::read_text_content;
use crate::indexer::extract::extract_document;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::Path;

pub const PROTOCOL_VERSION: &str = "2024-11-05";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const DEFAULT_PAGE_SIZE: usize = 10;
const MAX_PAGE_SIZE: usize = 50;
/// Result offsets are capped alongside the 10k exhaustive result cap. A cursor
/// beyond this point starts from scratch instead of materialising another
/// 10k-page in memory.
const MAX_OFFSET: usize = 10_000;
const SNIPPET_CHARS: usize = 100;
const SEARCH_TOKEN_BUDGET: usize = 1_500;

/// Serves MCP over stdin/stdout until EOF.
pub fn serve() -> Result<(), String> {
    // A missing index is not fatal: `initialize` must still answer, and tool
    // calls return a clear "no index" error until the GUI has run once.
    //
    // Prefer the daemon: it owns the only writer and already has the index
    // hot. Only fall back to the local read-only open when ping fails. The
    // ping is intentionally lightweight (no dir walk or DB load).
    let daemon_ready = crate::daemon::request("ping", json!({})).is_ok();
    let local = if daemon_ready {
        None
    } else {
        match HeadlessIndex::open() {
            Ok(index) => Some(index),
            Err(e) => {
                eprintln!("[Kensaku MCP] {}", e);
                None
            }
        }
    };
    let session = if daemon_ready {
        AgentSession::Daemon
    } else if let Some(index) = local.as_ref() {
        AgentSession::Local(index)
    } else {
        AgentSession::None
    };
    let mut reindex_enabled = KensakuConfig::load().mcp_allow_reindex;
    if reindex_enabled && !session.is_daemon() {
        eprintln!(
            "[Kensaku MCP] mcp_allow_reindex is set but no daemon is running; reindex stays disabled"
        );
        reindex_enabled = false;
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("Failed to read stdin: {}", e))?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle_request_session(&session, &request, reindex_enabled),
            Err(e) => Some(error_response(
                Value::Null,
                PARSE_ERROR,
                &format!("Parse error: {}", e),
            )),
        };

        if let Some(response) = response {
            let encoded = serde_json::to_string(&response)
                .map_err(|e| format!("Failed to encode response: {}", e))?;
            writeln!(out, "{}", encoded).map_err(|e| format!("Failed to write stdout: {}", e))?;
            out.flush()
                .map_err(|e| format!("Failed to flush stdout: {}", e))?;
        }
    }

    Ok(())
}

/// Compatibility wrapper for tests and local callers.
pub fn handle_request(index: Option<&HeadlessIndex>, request: &Value) -> Option<Value> {
    let session = match index {
        Some(index) => AgentSession::Local(index),
        None => AgentSession::None,
    };
    let reindex_enabled = index
        .map(|index| index.config.mcp_allow_reindex)
        .unwrap_or(false);
    handle_request_session(&session, request, reindex_enabled)
}

/// Handles one decoded JSON-RPC message for a daemon or local session.
///
/// Returns `None` for notifications (no reply is allowed). A `None` backend
/// keeps `initialize` usable while tool calls fail with a helpful message.
pub fn handle_request_session(
    session: &AgentSession<'_>,
    request: &Value,
    reindex_enabled: bool,
) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(error_response(
            Value::Null,
            PARSE_ERROR,
            "Parse error: request must be a JSON object",
        ));
    };

    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return id.map(|id| error_response(id, INVALID_REQUEST, "Invalid Request: missing method"));
    };

    // Notifications never get a reply (includes `notifications/initialized`).
    if method.starts_with("notifications/") {
        return None;
    }
    // No id means notification semantics even for request-like methods.
    let id = id?;

    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize_result()),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": tool_definitions(
                reindex_enabled,
                crate::embed::visual::visual_available(),
            )
        })),
        "tools/call" => call_tool(session, object.get("params"), reindex_enabled),
        other => Err((METHOD_NOT_FOUND, format!("Method not found: {}", other))),
    };

    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_response(id, code, &message),
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "kensaku",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Search the local Kensaku file index. Call tools/list to discover tools. Read-only by default; reindex requires mcp_allow_reindex.",
    })
}

fn tool_definitions(reindex_enabled: bool, visual_available: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "health_check",
            "description": "Check daemon, database, index and embedding-model health. Returns ok=false with a reason instead of failing the call.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search",
            "description": "Search the local Kensaku index (file names, paths and document contents). Read-only. Returns ranked results with bounded snippets and a cursor for the next page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query. Supports Kensaku syntax (ext:, in:, content:, re:, modified:)."
                    },
                    "page_size": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 10,
                        "description": "Results per page."
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Opaque next_cursor returned by the previous page."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Deprecated alias for page_size."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["ranked", "exhaustive"],
                        "default": "ranked",
                        "description": "Ranked order or exhaustive enumeration by path."
                    },
                    "path": {
                        "type": "string",
                        "description": "Only return results under this directory."
                    },
                    "ext": {
                        "type": "string",
                        "description": "Only return files with this extension (e.g. rs or .rs)."
                    },
                    "modified_after": {
                        "type": "string",
                        "description": "Only files modified within this duration (e.g. 1h, 7d, 30d)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "read_snippet",
            "description": "Read a bounded snippet from an indexed file, centered on the first case-insensitive match for an optional query.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the file to read."
                    },
                    "query": {
                        "type": "string",
                        "description": "Text to center the snippet on (case-insensitive)."
                    },
                    "max_tokens": {
                        "type": "integer",
                        "minimum": 32,
                        "maximum": 1024,
                        "default": 256,
                        "description": "Approximate token budget (about four characters per token)."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "index_state",
            "description": "Report index freshness/progress and size: file/dir counts, content documents, semantic units, model ids, phase and watched roots.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "regex_search",
            "description": "Bounded regular-expression search over indexed content. Read-only; returns the same pagination and snippet budget as search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Rust regex pattern (max 512 bytes)."
                    },
                    "page_size": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 10,
                        "description": "Results per page."
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Opaque next_cursor returned by the previous page."
                    },
                    "path": {
                        "type": "string",
                        "description": "Only return results under this directory."
                    },
                    "ext": {
                        "type": "string",
                        "description": "Only return files with this extension (e.g. rs or .rs)."
                    },
                    "modified_after": {
                        "type": "string",
                        "description": "Only files modified within this duration (e.g. 1h, 7d, 30d)."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
    ];
    tools.push(json!({
        "name": "find_symbol",
        "description": "Find code-symbol definitions (functions, classes, structs, traits, interfaces) by name across indexed source files. Candidates come from the content index; each file is parsed with tree-sitter. Read-only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Symbol name (case-insensitive; exact matches rank first)."
                },
                "language": {
                    "type": "string",
                    "enum": ["rust", "python", "javascript", "typescript"],
                    "description": "Restrict to one language. Optional."
                },
                "path": {
                    "type": "string",
                    "description": "Only search this file. Optional."
                },
                "page_size": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 10,
                    "description": "Maximum symbols to return."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }
    }));
    if visual_available {
        tools.push(json!({
            "name": "similar_images",
            "description": "Find visually similar images to a given indexed image path using visual vector embeddings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of the source image to compare."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 10,
                        "description": "Maximum number of similar images to return."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }));
    }
    if reindex_enabled {
        tools.push(json!({
            "name": "reindex",
            "description": "Opt-in write action: ask the daemon to start a full reindex. Only listed when mcp_allow_reindex is true in config.json; disabled by default.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }));
    }
    tools
}

#[derive(Debug)]
enum ToolFailure {
    /// Protocol-level invalid arguments (mapped to JSON-RPC -32602).
    Invalid(String),
    /// Tool ran but failed (returned as a tool result with `isError`).
    Execution(String),
}

fn call_tool(
    session: &AgentSession<'_>,
    params: Option<&Value>,
    reindex_enabled: bool,
) -> Result<Value, (i64, String)> {
    let Some(params) = params.and_then(Value::as_object) else {
        return Err((
            INVALID_PARAMS,
            "Invalid params: tools/call requires an object".to_string(),
        ));
    };

    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err((
            INVALID_PARAMS,
            "Invalid params: tool 'name' is required".to_string(),
        ));
    };

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome: Result<Value, ToolFailure> = match name {
        "health_check" => Ok(tool_health_check(session)),
        "search" => tool_search(session, &arguments),
        "read_snippet" => tool_read_snippet(session, &arguments),
        "index_state" => tool_index_state(session),
        "similar_images" => tool_similar_images(session, &arguments),
        "regex_search" => tool_regex_search(session, &arguments),
        "find_symbol" => match crate::agent::symbols::tool_find_symbol(session, &arguments) {
            Ok(payload) => Ok(payload),
            Err((message, invalid)) => {
                if invalid {
                    Err(ToolFailure::Invalid(message))
                } else {
                    Err(ToolFailure::Execution(message))
                }
            }
        },
        "reindex" => {
            if !reindex_enabled {
                Err(ToolFailure::Execution(
                    "reindex is disabled. Set \"mcp_allow_reindex\": true in config.json and restart the MCP server; it also requires the daemon."
                        .to_string(),
                ))
            } else {
                session
                    .reindex()
                    .map(|value| json!({ "started": value["started"].as_bool().unwrap_or(false), "as_of": now_unix_secs() }))
                    .map_err(ToolFailure::Execution)
            }
        }
        other => {
            return Err((INVALID_PARAMS, format!("Unknown tool: {}", other)));
        }
    };

    Ok(match outcome {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "isError": false,
        }),
        Err(ToolFailure::Invalid(message)) => return Err((INVALID_PARAMS, message)),
        Err(ToolFailure::Execution(message)) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    })
}

fn tool_health_check(session: &AgentSession<'_>) -> Value {
    let health = session.health();
    let state = health.get("state").cloned().unwrap_or_else(|| json!({}));
    let bool_field =
        |value: &Value, key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    let state_field = |key: &str| state.get(key).cloned().unwrap_or(Value::Null);

    json!({
        "ok": bool_field(&health, "ok"),
        "daemon": bool_field(&health, "daemon"),
        "protocol_version": health.get("protocol_version").cloned().unwrap_or(Value::Null),
        "db": bool_field(&health, "db"),
        "index": bool_field(&health, "index"),
        "model": bool_field(&health, "model"),
        "model_real": state_field("model_real"),
        "model_name": state_field("model_name"),
        "watcher_ok": bool_field(&health, "watcher_ok"),
        "files": state_field("files"),
        "content_docs": state_field("content_docs"),
        "semantic_units": state_field("semantic_units"),
        "phase": state_field("phase"),
        "is_indexing": state_field("is_indexing"),
        "paused": state_field("paused"),
        "error": health.get("error").cloned().unwrap_or(Value::Null),
        "as_of": now_unix_secs(),
    })
}

fn tool_search(session: &AgentSession<'_>, args: &Value) -> Result<Value, ToolFailure> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .ok_or_else(|| ToolFailure::Invalid("Invalid params: 'query' is required".to_string()))?;

    let mode = parse_mode(args)?;
    run_search(session, args, query, Some(query), &mode)
}

fn tool_regex_search(session: &AgentSession<'_>, args: &Value) -> Result<Value, ToolFailure> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .ok_or_else(|| ToolFailure::Invalid("Invalid params: 'pattern' is required".to_string()))?;
    if pattern.len() > 512 {
        return Err(ToolFailure::Invalid(
            "Invalid params: 'pattern' must be at most 512 bytes".to_string(),
        ));
    }
    if pattern.chars().any(char::is_control) {
        return Err(ToolFailure::Invalid(
            "Invalid params: 'pattern' must not contain control characters".to_string(),
        ));
    }
    regex::Regex::new(pattern)
        .map_err(|e| ToolFailure::Invalid(format!("Invalid params: invalid regex: {}", e)))?;

    let mode = parse_mode(args)?;
    // The lexer ends an unquoted value at the first whitespace, so a pattern
    // containing a space was split into a regex plus separate terms and the
    // caller silently got an AND of something they did not ask for.
    let composed = format!(
        "re:\"{}\"",
        pattern.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let snippet_query = regex_snippet_literal(pattern);
    let mut payload = run_search(session, args, &composed, snippet_query.as_deref(), &mode)?;
    if let Some(object) = payload.as_object_mut() {
        object.insert("pattern".to_string(), json!(pattern));
    }
    Ok(payload)
}

/// Best-effort literal for centering regex snippets. Alternations such as
/// `needle|omega` yield `needle`; symbolic patterns yield no snippet instead
/// of a misleading window.
fn regex_snippet_literal(pattern: &str) -> Option<String> {
    pattern.split('|').find_map(|part| {
        let literal = part.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-');
        (!literal.is_empty()
            && literal
                .chars()
                .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-'))
        .then(|| literal.to_string())
    })
}

fn run_search(
    session: &AgentSession<'_>,
    args: &Value,
    base_query: &str,
    snippet_query: Option<&str>,
    mode: &str,
) -> Result<Value, ToolFailure> {
    let page_size = parse_page_size(args)?;
    let cursor = parse_cursor(args)?;
    let offset = match cursor {
        Cursor::Offset(offset) => offset,
        // Keyset cursors only exist for exhaustive pages past the offset cap.
        Cursor::Path(_) if mode != "exhaustive" => {
            return Err(ToolFailure::Invalid(
                "Invalid params: keyset cursor requires mode:exhaustive".to_string(),
            ));
        }
        Cursor::Path(path) => {
            return run_search_keyset(session, args, base_query, snippet_query, page_size, &path);
        }
    };
    if offset > MAX_OFFSET {
        return Err(ToolFailure::Invalid(format!(
            "Invalid params: cursor is beyond the {} result pagination cap; use a keyset cursor (mode:exhaustive)",
            MAX_OFFSET
        )));
    }

    let path = optional_string(args, "path")?;
    let ext = optional_string(args, "ext")?;
    let modified_after = optional_string(args, "modified_after")?;

    // Agent-scope enforcement (read-only scoping per root): intersect the
    // requested path with the configured roots, or default to their union.
    // The policy fails closed, and caller-supplied `in:` scopes are rejected
    // when they fall outside the roots (the parser ORs includes together, so
    // appending roots alone would let a caller widen the search).
    let policy = session.policy().map_err(ToolFailure::Execution)?;
    policy
        .validate_query_scope(base_query)
        .map_err(ToolFailure::Invalid)?;
    let scope_paths = policy
        .query_scopes(path.as_deref())
        .map_err(ToolFailure::Invalid)?;
    let composed = compose_query_scoped(
        base_query,
        &scope_paths,
        ext.as_deref(),
        modified_after.as_deref(),
        mode == "exhaustive",
    );

    let response = session
        .search_page(&composed, page_size, offset)
        .map_err(ToolFailure::Execution)?;

    // Defense in depth: even a scoped query is filtered by the policy before
    // any path is returned to the model.
    // Ranked results already applied the offset in the dispatcher.
    // Exhaustive mode intentionally ignores the display limit, so slice here.
    let page: Vec<&crate::search::SearchResult> = if response.mode == "exhaustive" {
        response
            .results
            .iter()
            .skip(offset)
            .take(page_size)
            .filter(|result| policy.path_allowed(&result.path))
            .collect()
    } else {
        response
            .results
            .iter()
            .filter(|result| policy.path_allowed(&result.path))
            .collect()
    };

    let paths: Vec<String> = page.iter().map(|result| result.path.clone()).collect();
    let snippets = snippet_query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(|query| session.snippets(&paths, query))
        .unwrap_or_else(|| vec![None; paths.len()]);

    let mut snippet_budget_chars = SEARCH_TOKEN_BUDGET.saturating_mul(4);
    let results: Vec<Value> = page
        .iter()
        .enumerate()
        .map(|(idx, result)| {
            let full = result
                .snippet
                .as_deref()
                .or_else(|| snippets.get(idx).and_then(|snippet| snippet.as_deref()));
            let (snippet, snippet_truncated) = match full {
                None => (None, false),
                Some(_) if snippet_budget_chars == 0 => (None, true),
                Some(full) => {
                    let allowed = SNIPPET_CHARS.min(snippet_budget_chars);
                    let truncated = full.chars().count() > allowed;
                    let snippet = truncate_chars(full, allowed);
                    snippet_budget_chars = snippet_budget_chars.saturating_sub(allowed);
                    (Some(snippet), truncated)
                }
            };
            json!({
                "path": result.path,
                "score": result.score,
                "snippet": snippet,
                "snippet_truncated": snippet_truncated,
                "match_type": result.match_type,
                "size": result.file_size,
                "modified": result.modified,
            })
        })
        .collect();

    let returned = results.len();
    let consumed = offset.saturating_add(returned);
    let has_more = consumed < response.total;
    let cursor_limit_reached = has_more && consumed >= MAX_OFFSET;
    let next_cursor = if cursor_limit_reached {
        // Switch to keyset pagination: the next page starts after the last
        // path of this page instead of materialising another 10k offset.
        page.last()
            .map(|result| Value::String(encode_path_cursor(&result.path)))
            .unwrap_or(Value::Null)
    } else if has_more {
        Value::String(encode_cursor(consumed))
    } else {
        Value::Null
    };

    Ok(json!({
        "results": results,
        "returned": returned,
        "total": response.total,
        "mode": response.mode,
        "page_size": page_size,
        "next_cursor": next_cursor,
        "has_more": has_more,
        "cursor_limit_reached": cursor_limit_reached,
        "token_budget": SEARCH_TOKEN_BUDGET,
        "as_of": now_unix_secs(),
    }))
}

/// Exhaustive keyset page: matches strictly after `start_after`. The cap
/// applies per window, so arbitrarily large result sets stay reachable.
fn run_search_keyset(
    session: &AgentSession<'_>,
    args: &Value,
    base_query: &str,
    snippet_query: Option<&str>,
    page_size: usize,
    start_after: &str,
) -> Result<Value, ToolFailure> {
    let path = optional_string(args, "path")?;
    let ext = optional_string(args, "ext")?;
    let modified_after = optional_string(args, "modified_after")?;

    let policy = session.policy().map_err(ToolFailure::Execution)?;
    policy
        .validate_query_scope(base_query)
        .map_err(ToolFailure::Invalid)?;
    let scope_paths = policy
        .query_scopes(path.as_deref())
        .map_err(ToolFailure::Invalid)?;
    let composed = compose_query_scoped(
        base_query,
        &scope_paths,
        ext.as_deref(),
        modified_after.as_deref(),
        true,
    );

    let mut response = session
        .search_page_after(&composed, page_size, start_after)
        .map_err(ToolFailure::Execution)?;
    response
        .results
        .retain(|result| policy.path_allowed(&result.path));

    let paths: Vec<String> = response.results.iter().map(|r| r.path.clone()).collect();
    let snippets = snippet_query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(|query| session.snippets(&paths, query))
        .unwrap_or_else(|| vec![None; paths.len()]);

    let mut snippet_budget_chars = SEARCH_TOKEN_BUDGET.saturating_mul(4);
    let results: Vec<Value> = response
        .results
        .iter()
        .enumerate()
        .map(|(idx, result)| {
            let (snippet, snippet_truncated) = match result
                .snippet
                .as_deref()
                .or_else(|| snippets.get(idx).and_then(|s| s.as_deref()))
            {
                None => (None, false),
                Some(_) if snippet_budget_chars == 0 => (None, true),
                Some(full) => {
                    let allowed = SNIPPET_CHARS.min(snippet_budget_chars);
                    let truncated = full.chars().count() > allowed;
                    let snippet = truncate_chars(full, allowed);
                    snippet_budget_chars = snippet_budget_chars.saturating_sub(allowed);
                    (Some(snippet), truncated)
                }
            };
            json!({
                "path": result.path,
                "score": result.score,
                "snippet": snippet,
                "snippet_truncated": snippet_truncated,
                "match_type": result.match_type,
                "size": result.file_size,
                "modified": result.modified,
            })
        })
        .collect();

    let returned = results.len();
    let has_more = returned == page_size;
    let next_cursor = if has_more {
        response
            .results
            .last()
            .map(|result| Value::String(encode_path_cursor(&result.path)))
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    Ok(json!({
        "results": results,
        "returned": returned,
        // With a keyset the total is relative to the window: an honest lower
        // bound, not the whole set.
        "total": response.total,
        "total_is_lower_bound": response.total >= MAX_OFFSET,
        "mode": "exhaustive",
        "page_size": page_size,
        "next_cursor": next_cursor,
        "has_more": has_more,
        "cursor_limit_reached": false,
        "token_budget": SEARCH_TOKEN_BUDGET,
        "as_of": now_unix_secs(),
    }))
}

fn tool_read_snippet(session: &AgentSession<'_>, args: &Value) -> Result<Value, ToolFailure> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| ToolFailure::Invalid("Invalid params: 'path' is required".to_string()))?;

    // Scope enforcement applies to direct reads too, and secret-bearing files
    // stay unreadable even when the whole tree is in scope. The policy fails
    // closed, and the path must also be part of the index: a direct read must
    // never become an arbitrary-file reader for files the indexer skipped.
    let policy = session.policy().map_err(ToolFailure::Execution)?;
    if !policy.read_allowed(path) {
        return Err(ToolFailure::Invalid(
            "Invalid params: path is outside the configured agent scope".to_string(),
        ));
    }
    if !session
        .contains_path(path)
        .map_err(ToolFailure::Execution)?
    {
        return Err(ToolFailure::Invalid(
            "Invalid params: path is not part of the index".to_string(),
        ));
    }

    let query = optional_string(args, "query")?;

    let max_tokens = match args.get("max_tokens") {
        None | Some(Value::Null) => 256,
        Some(value) => value
            .as_u64()
            .filter(|tokens| (32..=1024).contains(tokens))
            .map(|tokens| tokens as usize)
            .ok_or_else(|| {
                ToolFailure::Invalid(
                    "Invalid params: 'max_tokens' must be an integer between 32 and 1024"
                        .to_string(),
                )
            })?,
    };

    // In daemon mode the daemon is the single source of truth. Never fall
    // back to a local read when it rejects the request: that would turn a
    // policy denial into a file read in this process.
    if session.is_daemon() {
        let value = crate::daemon::request(
            "read_snippet",
            json!({
                "path": path,
                "query": query,
                "max_tokens": max_tokens,
            }),
        )
        .map_err(ToolFailure::Execution)?;
        return Ok(value);
    }

    let file = Path::new(path);
    let text = extract_text(file)
        .ok_or_else(|| ToolFailure::Execution(format!("Could not read text from {}", path)))?;

    let (line_start, line_end, window, truncated) =
        read_snippet_window(&text, query.as_deref(), max_tokens);

    Ok(json!({
        "path": path,
        "line_start": line_start,
        "line_end": line_end,
        "text": window,
        "truncated": truncated,
        "token_budget": max_tokens,
    }))
}

fn tool_index_state(session: &AgentSession<'_>) -> Result<Value, ToolFailure> {
    let state = session.state().map_err(ToolFailure::Execution)?;
    let field = |key: &str| state.get(key).cloned().unwrap_or(Value::Null);

    Ok(json!({
        "files": field("files"),
        "dirs": field("dirs"),
        "content_docs": field("content_docs"),
        "semantic_units": field("semantic_units"),
        "index_bytes": field("index_bytes"),
        "model_id": field("model_id"),
        "active_model_id": field("active_model_id"),
        "model_name": field("model_name"),
        "model_real": field("model_real"),
        "roots": field("roots"),
        "last_reconcile": field("last_reconcile"),
        "phase": field("phase"),
        "is_indexing": field("is_indexing"),
        "paused": field("paused"),
        "files_indexed": field("files_indexed"),
        "content_indexed": field("content_indexed"),
        "extract_done": field("extract_done"),
        "extract_total": field("extract_total"),
        "as_of": now_unix_secs(),
    }))
}

fn tool_similar_images(session: &AgentSession<'_>, args: &Value) -> Result<Value, ToolFailure> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolFailure::Invalid("Invalid params: 'path' is required".to_string()))?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    // The source image is read to compute its embedding, so it is subject to
    // the same scope and denylist as any other direct read.
    let policy = session.policy().map_err(ToolFailure::Execution)?;
    if !policy.read_allowed(path) {
        return Err(ToolFailure::Invalid(
            "Invalid params: path is outside the configured agent scope".to_string(),
        ));
    }
    if !session
        .contains_path(path)
        .map_err(ToolFailure::Execution)?
    {
        return Err(ToolFailure::Invalid(
            "Invalid params: path is not part of the index".to_string(),
        ));
    }

    let results = session
        .find_similar_images(path, limit)
        .map_err(ToolFailure::Execution)?;

    Ok(json!({
        "path": path,
        "results": results,
    }))
}

fn parse_mode(args: &Value) -> Result<String, ToolFailure> {
    match args.get("mode") {
        None | Some(Value::Null) => Ok("ranked".to_string()),
        Some(Value::String(mode)) if mode == "ranked" || mode == "exhaustive" => Ok(mode.clone()),
        Some(_) => Err(ToolFailure::Invalid(
            "Invalid params: 'mode' must be \"ranked\" or \"exhaustive\"".to_string(),
        )),
    }
}

fn parse_page_size(args: &Value) -> Result<usize, ToolFailure> {
    for key in ["page_size", "limit"] {
        match args.get(key) {
            None | Some(Value::Null) => {}
            Some(value) => {
                return value
                    .as_u64()
                    .filter(|size| (1..=MAX_PAGE_SIZE as u64).contains(size))
                    .map(|size| size as usize)
                    .ok_or_else(|| {
                        ToolFailure::Invalid(format!(
                            "Invalid params: '{}' must be an integer between 1 and {}",
                            key, MAX_PAGE_SIZE
                        ))
                    });
            }
        }
    }
    Ok(DEFAULT_PAGE_SIZE)
}

fn parse_cursor(args: &Value) -> Result<Cursor, ToolFailure> {
    match args.get("cursor") {
        None | Some(Value::Null) => Ok(Cursor::Offset(0)),
        Some(Value::String(cursor)) => decode_cursor(cursor),
        Some(_) => Err(ToolFailure::Invalid(
            "Invalid params: 'cursor' must be a string".to_string(),
        )),
    }
}

fn encode_cursor(offset: usize) -> String {
    format!("k1-{:x}", offset)
}

/// Keyset cursor v2: hex of the last path, so exhaustive pages past the 10k
/// offset cap stay reachable. Paths are opaque bytes; hex is unambiguous.
fn encode_path_cursor(path: &str) -> String {
    let mut out = String::with_capacity(2 * path.len() + 3);
    out.push_str("k2-");
    for byte in path.as_bytes() {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

#[derive(Debug, PartialEq)]
enum Cursor {
    Offset(usize),
    Path(String),
}

fn decode_cursor(cursor: &str) -> Result<Cursor, ToolFailure> {
    if let Some(hex) = cursor.strip_prefix("k2-") {
        if hex.len() % 2 != 0 || hex.len() > 8_192 {
            return Err(ToolFailure::Invalid(
                "Invalid params: 'cursor' is malformed".to_string(),
            ));
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().chunks(2) {
            let value =
                u8::from_str_radix(std::str::from_utf8(pair).unwrap_or(""), 16).map_err(|_| {
                    ToolFailure::Invalid("Invalid params: 'cursor' is malformed".to_string())
                })?;
            bytes.push(value);
        }
        return String::from_utf8(bytes)
            .map(|path| Cursor::Path(path.trim_end_matches('/').to_string()))
            .map_err(|_| {
                ToolFailure::Invalid("Invalid params: 'cursor' is malformed".to_string())
            });
    }
    if cursor.len() > 32 {
        return Err(ToolFailure::Invalid(
            "Invalid params: 'cursor' is malformed".to_string(),
        ));
    }
    let Some(hex) = cursor.strip_prefix("k1-") else {
        return Err(ToolFailure::Invalid(
            "Invalid params: 'cursor' is malformed".to_string(),
        ));
    };
    usize::from_str_radix(hex, 16)
        .map(Cursor::Offset)
        .map_err(|_| ToolFailure::Invalid("Invalid params: 'cursor' is malformed".to_string()))
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, ToolFailure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolFailure::Invalid(format!(
            "Invalid params: '{}' must be a string",
            key
        ))),
    }
}

pub fn extract_text(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }

    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();

    const DOCUMENT_EXTENSIONS: &[&str] = &[
        "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "epub", "odt", "ods", "odp", "rtf",
        "eml", "msg",
    ];

    if DOCUMENT_EXTENSIONS.contains(&extension.as_str()) {
        extract_document(path, 1_048_576).map(|document| document.text)
    } else {
        read_text_content(path, 1_048_576)
    }
}

/// Returns `(line_start, line_end, text, truncated)` for a snippet centered on
/// the first case-insensitive `query` match, capped at `max_tokens * 4` chars.
///
/// Line numbers are 1-based and relative to the full extracted text.
pub fn read_snippet_window(
    text: &str,
    query: Option<&str>,
    max_tokens: usize,
) -> (usize, usize, String, bool) {
    let cap_chars = max_tokens.saturating_mul(4).max(1);

    let match_byte = query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .and_then(|query| text.to_lowercase().find(&query.to_lowercase()))
        .unwrap_or(0)
        .min(text.len());

    // Keep a little context before the match, then fill the rest of the budget
    // after it.
    let mut start = match_byte.saturating_sub(cap_chars / 4);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }

    let mut end = start;
    for (chars, (offset, ch)) in text[start..].char_indices().enumerate() {
        if chars >= cap_chars {
            break;
        }
        end = start + offset + ch.len_utf8();
    }

    let window = text[start..end].to_string();
    let line_start = text[..start].bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_end = line_start + window.bytes().filter(|byte| *byte == b'\n').count();
    let truncated = start > 0 || end < text.len();

    (line_start, line_end, window, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KensakuConfig;
    use crate::indexer::content::ContentIndex;
    use crate::indexer::metadata_db::MetadataDb;
    use crate::indexer::walker::FileEntry;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        index: Option<HeadlessIndex>,
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.index.take();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(tag: &str) -> Fixture {
        fixture_with_files(
            tag,
            &[(
                "notes.txt",
                "alpha line\nbeta line with needle\nomega line\n",
            )],
        )
    }

    fn fixture_with_files(tag: &str, files: &[(&str, &str)]) -> Fixture {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-mcp-test-{}-{}-{}-{}",
            tag,
            std::process::id(),
            seq,
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();

        let config = KensakuConfig {
            watched_dirs: vec![dir.clone()],
            ..KensakuConfig::default()
        };
        config.save_to(&dir).unwrap();

        let db = MetadataDb::open_in_dir(&dir).unwrap();
        {
            let index_dir = dir.join("index");
            let content_index = ContentIndex::new(&index_dir).unwrap();
            for (idx, (name, content)) in files.iter().enumerate() {
                let file = dir.join(name);
                fs::write(&file, content).unwrap();
                let modified = fs::metadata(&file)
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                db.upsert(&FileEntry {
                    id: idx as u64 + 1,
                    path: file.clone(),
                    name: name.to_string(),
                    extension: crate::indexer::walker::intern_extension(
                        Path::new(name).extension().and_then(|ext| ext.to_str()),
                    ),
                    size: content.len() as u64,
                    modified,
                    is_dir: false,
                    dev: 0,
                    ino: 0,
                    char_bloom: crate::search::fuzzy::compute_char_mask(name),
                    content_pending: false,
                })
                .unwrap();
                content_index
                    .add_or_update(
                        &file,
                        name,
                        content,
                        modified,
                        content.len() as u64,
                        Path::new(name).extension().and_then(|ext| ext.to_str()),
                    )
                    .unwrap();
            }
            content_index.commit().unwrap();
        }
        drop(db);

        let index = HeadlessIndex::open_at(&dir).expect("open headless index");
        Fixture {
            index: Some(index),
            dir,
        }
    }

    fn request(value: Value) -> Value {
        value
    }

    #[test]
    fn initialize_reports_protocol_and_server_info() {
        let fixture = fixture("initialize");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"})),
        )
        .expect("initialize must reply");

        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(response["result"]["serverInfo"]["name"], "kensaku");
        assert_eq!(
            response["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_reply() {
        let fixture = fixture("notifications");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
        );
        assert!(response.is_none());
    }

    #[test]
    fn tools_list_exposes_read_only_tools_and_valid_schemas() {
        let fixture = fixture("tools-list");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})),
        )
        .expect("tools/list must reply");

        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 7);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "health_check",
                "search",
                "read_snippet",
                "index_state",
                "regex_search",
                "find_symbol",
                "similar_images"
            ]
        );

        for tool in tools {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "tool {:?}", tool["name"]);
            assert!(schema["properties"].is_object());
            assert_eq!(schema["additionalProperties"], false);
        }
    }

    #[test]
    fn reindex_is_hidden_by_default_and_listed_when_enabled() {
        let fixture = fixture("reindex-list");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({"jsonrpc": "2.0", "id": 21, "method": "tools/list"})),
        )
        .expect("tools/list must reply");
        let tools = response["result"]["tools"].as_array().unwrap();
        assert!(!tools.iter().any(|tool| tool["name"] == "reindex"));

        let response = handle_request_session(
            &AgentSession::None,
            &request(json!({"jsonrpc": "2.0", "id": 22, "method": "tools/list"})),
            true,
        )
        .expect("tools/list must reply");
        let tools = response["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "reindex"));
    }

    #[test]
    fn tools_call_without_index_returns_is_error() {
        let response = handle_request(
            None,
            &request(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "search", "arguments": {"query": "needle"}}
            })),
        )
        .expect("tools/call must reply");

        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("No Kensaku index found"), "got: {text}");
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let fixture = fixture("unknown-method");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({"jsonrpc": "2.0", "id": 4, "method": "does/not/exist"})),
        )
        .expect("unknown method must reply");

        assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn search_tool_returns_results_and_truncates_snippets() {
        let fixture = fixture("search");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {"name": "search", "arguments": {"query": "needle", "limit": 5}}
            })),
        )
        .expect("tools/call must reply");

        assert_eq!(response["result"]["isError"], false);
        let payload: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload["results"].as_array().unwrap().len(), 1);
        assert!(
            payload["results"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("notes.txt")
        );
        assert!(
            payload["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("needle")
        );
        assert_eq!(payload["mode"], "ranked");
        assert!(payload["as_of"].as_u64().unwrap() > 0);
    }

    #[test]
    fn index_state_tool_reports_counts() {
        let fixture = fixture("state");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": {"name": "index_state", "arguments": {}}
            })),
        )
        .expect("tools/call must reply");

        let payload: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload["files"], 1);
        assert_eq!(payload["dirs"], 0);
        assert!(payload["index_bytes"].as_u64().unwrap() > 0);
        assert!(payload["as_of"].as_u64().unwrap() > 0);
        assert!(payload["model_id"].is_null());
        assert!(payload["active_model_id"].as_u64().unwrap() > 0);
        assert!(!payload["model_name"].as_str().unwrap().is_empty());
    }

    #[test]
    fn index_state_separates_units_model_from_active_model() {
        let fixture = fixture("model-ids");
        // Simulate vectors written with a different model id than the one the
        // process will embed new queries with.
        {
            let db = MetadataDb::open_in_dir(&fixture.dir).expect("open metadata db");
            db.insert_unit(1, 0, 10, 42, &vec![0i8; crate::embed::EMBEDDING_DIM])
                .expect("insert unit");
        }

        let state = fixture
            .index
            .as_ref()
            .expect("index open")
            .state()
            .expect("state");

        let active = crate::embed::ModelRegistry::global();
        assert_eq!(state["model_id"], 42);
        assert_eq!(state["active_model_id"], active.model_id);
        assert_eq!(state["model_name"], active.name);
    }

    #[test]
    fn invalid_arguments_are_invalid_params() {
        let fixture = fixture("invalid-params");
        let response = handle_request(
            fixture.index.as_ref(),
            &request(json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {"name": "search", "arguments": {"query": "x", "limit": 0}}
            })),
        )
        .expect("tools/call must reply");

        assert_eq!(response["error"]["code"], INVALID_PARAMS);
    }

    fn call(fixture: &Fixture, name: &str, arguments: Value) -> Value {
        handle_request(
            fixture.index.as_ref(),
            &request(json!({
                "jsonrpc": "2.0",
                "id": 100,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            })),
        )
        .expect("tools/call must reply")
    }

    fn payload(response: &Value) -> Value {
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn health_check_reports_local_health_without_error() {
        let fixture = fixture("health");
        let response = call(&fixture, "health_check", json!({}));
        assert_eq!(response["result"]["isError"], false);
        let payload = payload(&response);
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["daemon"], false);
        assert_eq!(payload["db"], true);
        assert_eq!(payload["index"], true);
        assert_eq!(payload["files"], 1);
        assert_eq!(payload["model"], true);
    }

    #[test]
    fn health_check_without_index_reports_unhealthy_not_tool_error() {
        let response = handle_request(
            None,
            &request(json!({
                "jsonrpc": "2.0",
                "id": 101,
                "method": "tools/call",
                "params": {"name": "health_check", "arguments": {}}
            })),
        )
        .expect("tools/call must reply");
        assert_eq!(response["result"]["isError"], false);
        let payload = payload(&response);
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["db"], false);
        assert!(
            payload["error"]
                .as_str()
                .unwrap()
                .contains("No Kensaku index")
        );
    }

    #[test]
    fn exhaustive_search_paginates_with_stable_cursor() {
        let fixture = fixture_with_files(
            "pagination",
            &[
                ("a.txt", "needle alpha"),
                ("b.txt", "needle beta"),
                ("c.txt", "needle gamma"),
                ("d.txt", "needle delta"),
                ("e.txt", "needle epsilon"),
            ],
        );

        let first = payload(&call(
            &fixture,
            "search",
            json!({"query": "needle", "mode": "exhaustive", "page_size": 2}),
        ));
        assert_eq!(first["total"], 5);
        assert_eq!(first["returned"], 2);
        assert_eq!(first["has_more"], true);
        let cursor = first["next_cursor"].as_str().unwrap().to_string();

        let second = payload(&call(
            &fixture,
            "search",
            json!({"query": "needle", "mode": "exhaustive", "page_size": 2, "cursor": cursor}),
        ));
        assert_eq!(second["returned"], 2);
        assert_eq!(second["has_more"], true);
        let cursor = second["next_cursor"].as_str().unwrap().to_string();

        let third = payload(&call(
            &fixture,
            "search",
            json!({"query": "needle", "mode": "exhaustive", "page_size": 2, "cursor": cursor}),
        ));
        assert_eq!(third["returned"], 1);
        assert_eq!(third["has_more"], false);
        assert!(third["next_cursor"].is_null());

        let mut paths: Vec<String> = Vec::new();
        for page in [&first, &second, &third] {
            for result in page["results"].as_array().unwrap() {
                paths.push(result["path"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(paths.len(), 5);
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 5);
    }

    #[test]
    fn exhaustive_keyset_cursor_pages_past_the_offset_cap() {
        let fixture = fixture_with_files(
            "keyset",
            &[
                ("a.txt", "needle alpha"),
                ("b.txt", "needle beta"),
                ("c.txt", "needle gamma"),
                ("d.txt", "needle delta"),
                ("e.txt", "needle epsilon"),
            ],
        );

        let first = payload(&call(
            &fixture,
            "search",
            json!({"query": "needle", "mode": "exhaustive", "page_size": 2}),
        ));
        assert_eq!(first["returned"], 2);
        let last_path = first["results"][1]["path"].as_str().unwrap().to_string();

        let second = payload(&call(
            &fixture,
            "search",
            json!({
                "query": "needle",
                "mode": "exhaustive",
                "page_size": 2,
                "cursor": encode_path_cursor(&last_path)
            }),
        ));
        let second_results = second["results"].as_array().unwrap();
        assert!(!second_results.is_empty());
        for result in second_results {
            let path = result["path"].as_str().unwrap();
            assert!(
                path > last_path.as_str(),
                "{} must follow {}",
                path,
                last_path
            );
        }
        assert_eq!(second["cursor_limit_reached"], false);
    }

    #[test]
    fn search_snippet_budget_marks_truncation() {
        let long = format!("{} needle {}", "x".repeat(200), "y".repeat(2_000));
        let fixture = fixture_with_files("snippet-budget", &[("long.txt", &long)]);
        let payload = payload(&call(
            &fixture,
            "search",
            json!({"query": "needle", "page_size": 5}),
        ));
        let result = &payload["results"][0];
        let snippet = result["snippet"].as_str().unwrap();
        assert!(snippet.chars().count() <= SNIPPET_CHARS);
        assert_eq!(result["snippet_truncated"], true);
        assert_eq!(payload["token_budget"], SEARCH_TOKEN_BUDGET);
    }

    #[test]
    fn regex_search_returns_results_and_rejects_invalid_patterns() {
        let fixture = fixture("regex");
        let response = call(&fixture, "regex_search", json!({"pattern": "needle|omega"}));
        assert_eq!(response["result"]["isError"], false);
        let payload = payload(&response);
        assert_eq!(payload["pattern"], "needle|omega");
        assert_eq!(payload["returned"], 1);
        assert!(
            payload["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("needle")
        );

        let response = call(&fixture, "regex_search", json!({"pattern": "["}));
        assert_eq!(response["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn reindex_tool_is_disabled_by_default() {
        let fixture = fixture("reindex-disabled");
        let response = call(&fixture, "reindex", json!({}));
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("reindex is disabled"), "got: {text}");
    }

    #[test]
    fn cursor_codec_rejects_malformed_and_caps_offsets() {
        assert_eq!(
            decode_cursor(&encode_cursor(1_234)).unwrap(),
            Cursor::Offset(1_234)
        );
        assert!(decode_cursor("nope").is_err());
        assert!(decode_cursor("k1-").is_err());
        assert!(decode_cursor("k1-zz").is_err());

        let request = json!({"cursor": encode_cursor(MAX_OFFSET + 1)});
        let error = run_search(&AgentSession::None, &request, "x", None, "ranked")
            .expect_err("offset beyond the cap must be rejected");
        match error {
            ToolFailure::Invalid(message) => assert!(message.contains("pagination cap")),
            ToolFailure::Execution(message) => panic!("unexpected execution error: {message}"),
        }
    }

    #[test]
    fn path_cursor_codec_roundtrips_and_rejects_garbage() {
        let cursor = encode_path_cursor("/tmp/some dir/file name.txt");
        assert_eq!(
            decode_cursor(&cursor).unwrap(),
            Cursor::Path("/tmp/some dir/file name.txt".to_string())
        );
        assert!(decode_cursor("k2-abc").is_err(), "odd hex rejected");
        assert!(decode_cursor("k2-zzzz").is_err(), "non-hex rejected");
        // A keyset cursor outside exhaustive mode is rejected.
        let error = run_search(
            &AgentSession::None,
            &json!({"cursor": cursor}),
            "x",
            None,
            "ranked",
        )
        .expect_err("keyset cursor needs exhaustive mode");
        match error {
            ToolFailure::Invalid(message) => assert!(message.contains("keyset")),
            ToolFailure::Execution(message) => panic!("unexpected execution error: {message}"),
        }
    }

    #[test]
    #[ignore = "reports warm MCP search p50/p95; run with --ignored --nocapture"]
    fn mcp_search_latency() {
        let names: Vec<String> = (0..100).map(|i| format!("doc-{i}.txt")).collect();
        let contents: Vec<String> = (0..100)
            .map(|i| format!("alpha beta gamma needle line {i}\n"))
            .collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .zip(contents.iter())
            .map(|(name, content)| (name.as_str(), content.as_str()))
            .collect();
        let fixture = fixture_with_files("latency", &files);

        let mut samples = Vec::with_capacity(200);
        for _ in 0..200 {
            let start = std::time::Instant::now();
            let response = handle_request(
                fixture.index.as_ref(),
                &request(json!({
                    "jsonrpc": "2.0",
                    "id": 200,
                    "method": "tools/call",
                    "params": {
                        "name": "search",
                        "arguments": {"query": "needle", "page_size": 10}
                    }
                })),
            )
            .expect("search must reply");
            std::hint::black_box(response);
            samples.push(start.elapsed().as_micros() as u64);
        }
        samples.sort_unstable();
        println!(
            "K8 MCP search latency over 100 files: p50={}µs p95={}µs",
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100]
        );
    }

    #[test]
    fn malformed_cursor_and_unknown_tool_are_invalid_params() {
        let fixture = fixture("bad-cursor");
        let response = call(
            &fixture,
            "search",
            json!({"query": "needle", "cursor": "not-a-cursor"}),
        );
        assert_eq!(response["error"]["code"], INVALID_PARAMS);

        let response = call(&fixture, "does_not_exist", json!({}));
        assert_eq!(response["error"]["code"], INVALID_PARAMS);

        let response = call(
            &fixture,
            "search",
            json!({"query": "needle", "page_size": 51}),
        );
        assert_eq!(response["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn read_snippet_windowing_finds_match_and_reports_lines() {
        let mut text = String::new();
        for i in 1..=100 {
            if i == 50 {
                text.push_str("the NEEDLE is here\n");
            } else {
                text.push_str(&format!("line {}\n", i));
            }
        }

        let (line_start, line_end, window, truncated) =
            read_snippet_window(&text, Some("needle"), 32);

        assert!(window.to_lowercase().contains("needle"));
        assert!(window.chars().count() <= 32 * 4);
        assert!(truncated);
        assert!(
            line_start <= 50 && line_end >= 50,
            "{line_start}..{line_end}"
        );
        assert!(line_start > 1);
    }

    #[test]
    fn read_snippet_without_query_starts_at_file_start() {
        let text = "hello world\nsecond line\n";
        let (line_start, line_end, window, truncated) = read_snippet_window(text, None, 256);
        assert_eq!(line_start, 1);
        assert_eq!(line_end, 3);
        assert_eq!(window, text);
        assert!(!truncated);
    }

    #[test]
    fn similar_images_tool_registered_and_handles_missing_path() {
        let fixture = fixture("similar-tool");
        let tools_resp = handle_request(
            fixture.index.as_ref(),
            &request(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
            })),
        )
        .expect("tools/list must reply");

        let tools = tools_resp["result"]["tools"]
            .as_array()
            .expect("tools array");
        assert!(
            tools.iter().any(|t| t["name"] == "similar_images"),
            "similar_images must be listed in tools/list"
        );

        // Missing required 'path' parameter returns INVALID_PARAMS
        let call_resp = call(&fixture, "similar_images", json!({}));
        assert_eq!(call_resp["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn read_snippet_refuses_unindexed_files() {
        let fixture = fixture("read-unindexed");
        let outside = fixture.dir.join("outside.txt");
        fs::write(&outside, "not part of the index").unwrap();

        let response = call(
            &fixture,
            "read_snippet",
            json!({ "path": outside.to_string_lossy() }),
        );
        assert_eq!(response["error"]["code"], INVALID_PARAMS);
        let text = response["error"]["message"].as_str().unwrap();
        assert!(
            text.contains("not part of the index"),
            "unindexed file must be refused, got: {text}"
        );
    }

    #[test]
    fn read_snippet_refuses_indexed_credentials() {
        let fixture = fixture_with_files("read-env", &[(".env.production", "SECRET=1")]);
        let path = fixture.dir.join(".env.production");

        let response = call(
            &fixture,
            "read_snippet",
            json!({ "path": path.to_string_lossy() }),
        );
        assert_eq!(response["error"]["code"], INVALID_PARAMS);
        let text = response["error"]["message"].as_str().unwrap();
        assert!(
            text.contains("outside the configured agent scope"),
            "credential file must be refused even when indexed, got: {text}"
        );
    }

    #[test]
    fn read_snippet_returns_indexed_files() {
        let fixture = fixture("read-indexed");
        let path = fixture.dir.join("notes.txt");

        let response = call(
            &fixture,
            "read_snippet",
            json!({ "path": path.to_string_lossy(), "max_tokens": 64 }),
        );
        assert_eq!(response["result"]["isError"], false);
        let payload = payload(&response);
        assert!(payload["text"].as_str().unwrap().contains("alpha"));
    }
}
