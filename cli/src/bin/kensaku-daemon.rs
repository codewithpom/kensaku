use interprocess::local_socket::prelude::*;
use kensaku_lib::config::KensakuConfig;
use kensaku_lib::indexer::IndexerManager;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn socket_path(config_dir: &Path) -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        Path::new(&runtime).join("kensaku.sock")
    } else {
        config_dir.join("kensaku.sock")
    }
}

#[cfg(unix)]
fn socket_name(config_dir: &Path) -> std::io::Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    socket_path(config_dir).to_fs_name::<GenericFilePath>()
}

#[cfg(windows)]
fn socket_name(_config_dir: &Path) -> std::io::Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName};
    "kensaku".to_ns_name::<GenericNamespaced>()
}

fn token_path(config_dir: &Path) -> PathBuf {
    config_dir.join("daemon.token")
}

fn ensure_token(config_dir: &Path) -> std::io::Result<String> {
    let path = token_path(config_dir);
    if let Ok(existing) = fs::read_to_string(&path) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    // Generate a simple token from random-ish bytes using process id + time.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let token = format!("{:x}-{:x}", std::process::id(), nanos);
    fs::write(&path, &token)?;
    #[cfg(unix)]
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(token)
}

#[cfg(unix)]
mod unix_cleanup {
    use std::ffi::CString;
    use std::sync::OnceLock;

    static SOCKET_C: OnceLock<CString> = OnceLock::new();
    static PID_C: OnceLock<CString> = OnceLock::new();

    pub fn remember(socket: &std::path::Path, pid: &std::path::Path) {
        if let Ok(c) = CString::new(socket.to_string_lossy().as_bytes()) {
            let _ = SOCKET_C.set(c);
        }
        if let Ok(c) = CString::new(pid.to_string_lossy().as_bytes()) {
            let _ = PID_C.set(c);
        }
    }

    pub fn cleanup_paths() {
        if let Some(path) = SOCKET_C.get() {
            unsafe { libc::unlink(path.as_ptr()) };
        }
        if let Some(path) = PID_C.get() {
            unsafe { libc::unlink(path.as_ptr()) };
        }
    }

    extern "C" fn handle_term(_sig: libc::c_int) {
        // `unlink` is async-signal-safe; paths were precomputed at startup.
        cleanup_paths();
        std::process::exit(0);
    }

    pub fn install_signal_handlers() {
        unsafe {
            libc::signal(
                libc::SIGTERM,
                handle_term as *const () as libc::sighandler_t,
            );
            libc::signal(libc::SIGINT, handle_term as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(unix)]
fn cleanup_paths() {
    unix_cleanup::cleanup_paths();
}

#[cfg(not(unix))]
fn cleanup_paths() {}

#[cfg(unix)]
fn install_signal_handlers() {
    unix_cleanup::install_signal_handlers();
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

fn main() {
    install_signal_handlers();
    let config_dir = KensakuConfig::config_dir();
    let _ = fs::create_dir_all(&config_dir);
    let token = match ensure_token(&config_dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("kensaku-daemon: token: {}", e);
            std::process::exit(1);
        }
    };

    // Single instance lock
    let lock_path = config_dir.join("kensaku.lock");
    let lock_file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("kensaku-daemon: lock: {}", e);
            std::process::exit(1);
        }
    };
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(_) => {
            eprintln!("kensaku-daemon: another daemon is running; exiting");
            std::process::exit(0);
        }
    }

    #[cfg(unix)]
    let sock = socket_path(&config_dir);
    #[cfg(unix)]
    let _ = fs::remove_file(&sock);
    let name = match socket_name(&config_dir) {
        Ok(name) => name,
        Err(e) => {
            eprintln!("kensaku-daemon: socket name: {}", e);
            std::process::exit(1);
        }
    };
    let listener = match interprocess::local_socket::ListenerOptions::new()
        .name(name)
        .create_sync()
    {
        Ok(l) => l,
        Err(e) => {
            eprintln!("kensaku-daemon: bind: {}", e);
            std::process::exit(1);
        }
    };
    #[cfg(unix)]
    let _ = fs::set_permissions(&sock, fs::Permissions::from_mode(0o600));

    let pid_path = config_dir.join("daemon.pid");
    let _ = fs::write(&pid_path, std::process::id().to_string());
    #[cfg(unix)]
    unix_cleanup::remember(&sock, &pid_path);

    // Start indexer
    let cfg = KensakuConfig::load();
    let manager = Arc::new(IndexerManager::new(cfg));
    {
        // OCR the already-indexed images once the PP-OCR pack lands.
        let manager = Arc::clone(&manager);
        kensaku_lib::models::on_ocr_pack_ready(move || manager.start_indexing_when_idle());
    }
    manager.start_indexing();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let token = token.clone();
                let manager = Arc::clone(&manager);
                std::thread::spawn(move || handle_client(stream, &token, &manager));
            }
            Err(_) => continue,
        }
    }
}

/// Whether `path` is part of the index the daemon serves.
///
/// Checks the exact spelling first, then the canonicalized form so a symlink
/// or a path with `.`/`..` still resolves to an indexed entry.
fn is_indexed(manager: &Arc<IndexerManager>, path: &str) -> bool {
    let requested = Path::new(path);
    let metadata = manager.metadata.read();
    metadata.get(requested).is_some()
        || std::fs::canonicalize(requested)
            .ok()
            .is_some_and(|canonical| metadata.get(&canonical).is_some())
}

fn handle_client(stream: LocalSocketStream, token: &str, manager: &Arc<IndexerManager>) {
    let buf = BufReader::new(&stream);
    let mut writer = &stream;
    for line in buf.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}})
                );
                continue;
            }
        };
        // Handshake/auth: require params.token
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let provided = params.get("token").and_then(|t| t.as_str());
        let protocol_version = params.get("protocol_version").and_then(|v| v.as_u64());
        if let Err((code, message)) =
            kensaku_lib::daemon::authenticate(token, provided, protocol_version)
        {
            let _ = writeln!(
                writer,
                "{}",
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            );
            continue;
        }

        let result = match method {
            "ping" => Ok(json!({"ok": true, "protocol_version": 1})),
            "health" => match manager.state_json() {
                Ok(state) => Ok(json!({
                    "ok": true,
                    "daemon": true,
                    "protocol_version": 1,
                    "pid": std::process::id(),
                    "watcher_ok": manager.watcher_ok(),
                    "db": true,
                    "index": state["content_index_ready"].as_bool().unwrap_or(false),
                    "model": state["active_model_id"].as_u64().is_some(),
                    "state": state,
                })),
                Err(e) => Ok(json!({
                    "ok": true,
                    "daemon": true,
                    "protocol_version": 1,
                    "pid": std::process::id(),
                    "watcher_ok": manager.watcher_ok(),
                    "db": false,
                    "index": false,
                    "model": false,
                    "error": e,
                })),
            },
            "get_stats" => kensaku_lib::commands::stats_for(manager),
            "get_home_data" => kensaku_lib::commands::home_data_for(manager)
                .and_then(|data| serde_json::to_value(data).map_err(|e| e.to_string())),
            "toggle_pin" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    Err("toggle_pin requires a path".to_string())
                } else {
                    kensaku_lib::commands::toggle_pin_for(manager, path.to_string())
                        .map(|pinned| json!(pinned))
                }
            }
            "get_scope_suggestions" => {
                let prefix = params
                    .get("prefix")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let limit = params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                kensaku_lib::commands::scope_suggestions_for(manager, prefix, limit)
                    .and_then(|items| serde_json::to_value(items).map_err(|e| e.to_string()))
            }
            "find_duplicates" => {
                let max_distance = params
                    .get("max_distance")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let limit = params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                kensaku_lib::commands::duplicates_for(manager, max_distance, limit)
                    .and_then(|groups| serde_json::to_value(groups).map_err(|e| e.to_string()))
            }
            "find_similar_images" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                if path.is_empty() {
                    Err("find_similar_images requires a path".to_string())
                } else {
                    manager
                        .find_similar_images(std::path::Path::new(path), limit)
                        .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
                }
            }
            "get_status" => serde_json::to_value(manager.get_status()).map_err(|e| e.to_string()),
            "get_config" => {
                serde_json::to_value(manager.config.read().clone()).map_err(|e| e.to_string())
            }
            "save_config" => {
                let config: KensakuConfig = match serde_json::from_value(
                    params
                        .get("config")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ) {
                    Ok(config) => config,
                    Err(e) => {
                        let _ = writeln!(
                            writer,
                            "{}",
                            serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.to_string()}})
                        );
                        continue;
                    }
                };
                match manager.apply_config(config) {
                    Ok(()) => Ok(serde_json::json!({"ok": true})),
                    Err(e) => Err(e),
                }
            }
            "reindex" => {
                if !manager.config.read().mcp_allow_reindex {
                    Err("reindex is disabled; enable mcp_allow_reindex in config.json".to_string())
                } else {
                    manager.start_indexing();
                    Ok(json!({"started": true}))
                }
            }
            "rescan" => {
                // GUI-initiated re-scan of the watched folders. Unlike the
                // MCP `reindex` tool this is not gated on
                // `mcp_allow_reindex`: it is the user speaking from the
                // app's own UI, not an agent.
                //
                // Reload the config the GUI just saved (setup completion,
                // wizard changes): crawling the previous in-memory config
                // would either use stale watched dirs or, worse, the default
                // pre-setup roots if setup finished while the daemon was up.
                let _ = manager.apply_config(KensakuConfig::load());
                manager.start_indexing();
                Ok(json!({"started": true}))
            }
            "search" => {
                let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let with_snippets = params
                    .get("snippets")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if let Some(start_after) = params.get("start_after").and_then(|v| v.as_str()) {
                    manager
                        .search_page_after(query, limit, start_after, with_snippets)
                        .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
                } else {
                    manager
                        .search_page(query, limit, offset, with_snippets)
                        .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
                }
            }
            "get_snippets" => {
                let paths: Vec<String> = params
                    .get("paths")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
                serde_json::to_value(kensaku_lib::commands::compute_snippets(paths, query))
                    .map_err(|e| e.to_string())
            }
            "read_snippet" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    Err("read_snippet requires a path".to_string())
                } else if !{
                    // The daemon is reachable by every local MCP client, so it
                    // must enforce the same scope/denylist rules as the
                    // in-process agent path. It used to read any file the
                    // caller named.
                    let cfg = manager.config.read();
                    kensaku_lib::agent::agent_read_allowed(
                        &cfg.agent_scoped_roots,
                        &cfg.excluded_extensions,
                        path,
                    )
                } {
                    Err("read_snippet: path is outside the configured agent scope".to_string())
                } else if !is_indexed(manager, path) {
                    Err("read_snippet: path is not part of the index".to_string())
                } else {
                    let query = params.get("query").and_then(|v| v.as_str());
                    let max_tokens = params
                        .get("max_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(256)
                        .clamp(32, 1024) as usize;
                    match kensaku_lib::agent::mcp::extract_text(Path::new(path)) {
                        Some(text) => {
                            let (line_start, line_end, window, truncated) =
                                kensaku_lib::agent::mcp::read_snippet_window(
                                    &text, query, max_tokens,
                                );
                            Ok(json!({
                                "path": path,
                                "line_start": line_start,
                                "line_end": line_end,
                                "text": window,
                                "truncated": truncated,
                            }))
                        }
                        None => Err(format!("Could not read text from {}", path)),
                    }
                }
            }
            "contains_path" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    Err("contains_path requires a path".to_string())
                } else {
                    Ok(json!({ "indexed": is_indexed(manager, path) }))
                }
            }
            "index_state" => manager.state_json(),
            "shutdown" => {
                cleanup_paths();
                std::process::exit(0);
            }
            _ => Err(format!("unknown method {}", method)),
        };

        let resp = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e}}),
        };
        let _ = writeln!(writer, "{}", resp);
    }
}
