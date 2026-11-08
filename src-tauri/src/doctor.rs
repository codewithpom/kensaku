//! On-demand health checks for the running application.
//!
//! Every check is best-effort: failures are reported as statuses instead of
//! being propagated as errors, and no check is allowed to panic.

use crate::AppState;
use crate::config::{ExternalExtractor, KensakuConfig};
use crate::indexer::extract::external;
use crate::models;
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub label: String,
    pub status: String,
    pub detail: String,
    pub fix: Option<String>,
    /// What the UI can do about it, when there is something to do.
    ///
    /// The fix text alone was a dead end: the models row said "download model
    /// files in AI & Semantic", and that panel has no download button, so the
    /// instruction could not be followed. A check that knows how to repair
    /// itself should offer that, not a sentence to copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<DoctorAction>,
}

/// A repair the Health page can perform, named rather than described.
///
/// String kinds rather than a closed enum in the JSON: the frontend switches on
/// them, and an unknown kind is simply not rendered as a button, so an older UI
/// talking to a newer backend degrades instead of breaking.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorAction {
    /// Stable identifier: `download-models`, `download-ocr-pack`,
    /// `restart-watcher`, `cleanup-storage`, `open-crash-log`,
    /// `reindex`, `open-settings`.
    pub kind: String,
    pub label: String,
    /// Settings section to open, for `open-settings`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
}

impl DoctorAction {
    fn new(kind: &str, label: &str) -> Self {
        Self {
            kind: kind.to_string(),
            label: label.to_string(),
            section: None,
        }
    }

}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub ran_at: u64,
    pub summary: DoctorSummary,
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn check(id: &str, label: &str, status: &str, detail: String, fix: Option<String>) -> DoctorCheck {
    DoctorCheck {
        id: id.to_string(),
        label: label.to_string(),
        status: status.to_string(),
        detail,
        fix,
        action: None,
    }
}

/// Attaches a repair action to a check.
fn with_action(mut check: DoctorCheck, action: DoctorAction) -> DoctorCheck {
    check.action = Some(action);
    check
}

fn ok(id: &str, label: &str, detail: String) -> DoctorCheck {
    check(id, label, "ok", detail, None)
}

fn warn(id: &str, label: &str, detail: String, fix: Option<String>) -> DoctorCheck {
    check(id, label, "warn", detail, fix)
}

fn fail(id: &str, label: &str, detail: String, fix: Option<String>) -> DoctorCheck {
    check(id, label, "fail", detail, fix)
}

fn is_locked(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("locked") || lower.contains("busy")
}

/// Recursively sums regular files below `dir`. Symlinks are not followed.
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

fn dir_is_empty(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{} B", bytes)
    }
}

/// "just now", "12 min ago", "3 h ago", "2 d ago".
fn format_age(at_unix_secs: u64) -> String {
    let now = now_unix_secs();
    let secs = now.saturating_sub(at_unix_secs);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

fn check_config_dir_writable() -> DoctorCheck {
    const ID: &str = "config_dir";
    const LABEL: &str = "Config directory writable";
    let dir = KensakuConfig::config_dir();

    let probe = dir.join(format!(".doctor-write-probe-{}", std::process::id()));
    let result = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(&probe, b"kensaku"))
        .and_then(|_| std::fs::remove_file(&probe));

    match result {
        Ok(()) => ok(ID, LABEL, format!("Writable: {}", dir.display())),
        Err(e) => fail(
            ID,
            LABEL,
            format!("Not writable ({}): {}", dir.display(), e),
            Some("Check ownership and permissions on your config directory.".to_string()),
        ),
    }
}

fn check_disk_space(config_dir: &Path) -> DoctorCheck {
    const ID: &str = "disk_space";
    const LABEL: &str = "Disk space";
    const MIB: u64 = 1024 * 1024;

    match fs4::available_space(config_dir) {
        Ok(bytes) => {
            let detail = format!(
                "{} available for the index, thumbnails and model packs.",
                format_bytes(bytes)
            );
            if bytes < 100 * MIB {
                fail(
                    ID,
                    LABEL,
                    detail,
                    Some("Free disk space before indexing; writes will fail soon.".to_string()),
                )
            } else if bytes < 1024 * MIB {
                warn(
                    ID,
                    LABEL,
                    detail,
                    Some(
                        "Consider freeing space before a large reindex or model download."
                            .to_string(),
                    ),
                )
            } else {
                ok(ID, LABEL, detail)
            }
        }
        Err(e) => warn(
            ID,
            LABEL,
            format!(
                "Could not read free space for {}: {}",
                config_dir.display(),
                e
            ),
            None,
        ),
    }
}

fn check_crash_log() -> DoctorCheck {
    const ID: &str = "crash_log";
    const LABEL: &str = "Crash log";

    let path = crate::crashlog::path();

    // Only actual failures count. The file existing is not evidence of a
    // crash: older builds logged routine startup milestones here, and a
    // healthy install could show a warning whose text was the app's own
    // "config loaded" line.
    match crate::crashlog::last_failure() {
        Some(line) => with_action(
            warn(
                ID,
                LABEL,
                line,
                // The fix is intentionally just the path: the UI opens it in
                // the user's editor rather than interpreting the crash.
                Some(path.display().to_string()),
            ),
            DoctorAction::new("open-crash-log", "Open log"),
        ),
        None => ok(ID, LABEL, "No crash log entries.".to_string()),
    }
}

fn check_metadata_db(config_dir: &Path) -> DoctorCheck {
    const ID: &str = "metadata_db";
    const LABEL: &str = "Metadata database";
    let path = config_dir.join("metadata.db");

    if !path.is_file() {
        return warn(
            ID,
            LABEL,
            "No metadata database yet (nothing indexed).".to_string(),
            None,
        );
    }

    let conn = match rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        Err(e) => {
            let message = e.to_string();
            if is_locked(&message) {
                return warn(ID, LABEL, "index in use".to_string(), None);
            }
            return fail(
                ID,
                LABEL,
                format!("Cannot open database: {}", message),
                Some("Reset Kensaku data from Settings if the database is corrupt.".to_string()),
            );
        }
    };

    let quick_check: Result<String, _> = conn.query_row("PRAGMA quick_check", [], |row| row.get(0));

    let quick_check = match quick_check {
        Ok(value) => value,
        Err(e) => {
            let message = e.to_string();
            if is_locked(&message) {
                return warn(ID, LABEL, "index in use".to_string(), None);
            }
            return fail(
                ID,
                LABEL,
                format!("quick_check failed: {}", message),
                Some("Reset Kensaku data from Settings to rebuild the index.".to_string()),
            );
        }
    };

    if !quick_check.eq_ignore_ascii_case("ok") {
        return fail(
            ID,
            LABEL,
            format!("quick_check reported: {}", quick_check),
            Some("Reset Kensaku data from Settings to rebuild the index.".to_string()),
        );
    }

    let nodes = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get::<_, i64>(0))
        .ok()
        .map(|count| count.max(0) as u64);

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let detail = match nodes {
        Some(count) => format!("quick_check ok · {} nodes · {}", count, format_bytes(size)),
        None => format!("quick_check ok · {}", format_bytes(size)),
    };

    ok(ID, LABEL, detail)
}

fn check_full_text_index(nodes: usize) -> DoctorCheck {
    const ID: &str = "full_text_index";
    const LABEL: &str = "Full-text index";
    let dir = KensakuConfig::index_dir();

    if !dir.is_dir() {
        return warn(
            ID,
            LABEL,
            "Index directory not created yet.".to_string(),
            None,
        );
    }

    let bytes = dir_size_bytes(&dir);
    if dir_is_empty(&dir) && nodes > 0 {
        return with_action(
            fail(
                ID,
                LABEL,
                format!(
                    "Index directory is empty while {} nodes are indexed.",
                    nodes
                ),
                None,
            ),
            DoctorAction::new("reindex", "Rebuild the index"),
        );
    }

    let schema_marker = dir.join("content.schema");
    let schema_ver = std::fs::read_to_string(&schema_marker)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let ver_label = match schema_ver {
        Some(v) => format!("schema v{v} (kensaku-cjk)"),
        None => "schema v1 (legacy)".to_string(),
    };

    let legacy_backups = dir
        .parent()
        .and_then(|p| std::fs::read_dir(p).ok())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("index.legacy-"))
                .count()
        })
        .unwrap_or(0);

    let detail = if legacy_backups > 0 {
        format!(
            "{} · {} · {} ({} legacy backup)",
            format_bytes(bytes),
            ver_label,
            dir.display(),
            legacy_backups
        )
    } else {
        format!(
            "{} · {} · {}",
            format_bytes(bytes),
            ver_label,
            dir.display()
        )
    };

    ok(ID, LABEL, detail)
}

/// What the data folder holds, and whether past upgrades left anything behind.
fn check_storage() -> DoctorCheck {
    let info = crate::maintenance::inventory();
    let mut parts = vec![format!("data folder {}", format_bytes(info.total_bytes))];
    if info.index_bytes > 0 {
        parts.push(format!("index {}", format_bytes(info.index_bytes)));
    }
    if info.pack_bytes > 0 {
        parts.push(format!("downloaded packs {}", format_bytes(info.pack_bytes)));
    }
    if info.legacy_backups > 0 {
        parts.push(format!(
            "{} superseded index backup(s), {}",
            info.legacy_backups,
            format_bytes(info.legacy_backup_bytes)
        ));
    }
    if info.update_temp_bytes > 0 {
        parts.push(format!("staged updates {}", format_bytes(info.update_temp_bytes)));
    }
    let detail = parts.join(" · ");

    // Old schema backups and staged installers are pure accumulation. They are
    // pruned at startup, but say so rather than leaving a user to wonder why
    // their profile keeps growing.
    if info.legacy_backups > 1 || info.update_temp_bytes > 0 {
        return with_action(
            warn("storage", "Data folder", detail, None),
            DoctorAction::new("cleanup-storage", "Reclaim now"),
        );
    }
    ok("storage", "Data folder", detail)
}

/// What the last crawl did, so "it indexes every time I open it" can be
/// answered from the Health tab rather than from output a release build never
/// shows anyone.
fn check_last_crawl() -> DoctorCheck {
    let Some(report) = crate::indexer::last_crawl_report() else {
        return ok(
            "last_crawl",
            "Last index scan",
            "No crawl recorded yet.".to_string(),
        );
    };

    let when = format_age(report.at);
    let mut detail = format!(
        "{}: scanned {} entries, re-read {}, reason: {}",
        when, report.walked, report.reindexed, report.reason
    );
    if report.retried > 0 {
        detail.push_str(&format!(", {} pending a retry", report.retried));
    }
    if report.abandoned > 0 {
        detail.push_str(&format!(
            ", {} given up on after repeated failures",
            report.abandoned
        ));
    }
    // When the OCR state differs from the recorded one, every image and
    // scanned PDF is re-read. Saying so turns a mysterious multi-minute scan
    // into a named cause.
    if report.ocr_signature_stored.as_deref() != Some(report.ocr_signature_now.as_str()) {
        detail.push_str(&format!(
            " · OCR state {} -> {}",
            report
                .ocr_signature_stored
                .as_deref()
                .unwrap_or("<none recorded>"),
            report.ocr_signature_now
        ));
    }

    // A launch that re-reads a large share of the corpus without a schema
    // rebuild or an OCR change is the pattern worth flagging.
    if is_heavy_scan(&report) {
        return warn(
            "last_crawl",
            "Last index scan",
            detail,
            Some("Report this: the scan re-read most of the corpus, which it should only do once after an upgrade.".to_string()),
        );
    }

    ok("last_crawl", "Last index scan", detail)
}

/// Whether a crawl re-read a large share of the corpus for no stated reason.
///
/// A first scan — or a crawl over an index that was just recreated — re-reads
/// everything by definition, and those files are reported as new. That is
/// expected, so it must not surface as a warning asking the user to file a
/// bug: every fresh install would see one (#142, #146). Only files the index
/// had already seen are worth flagging, and only when they are not explained
/// by a content-index rebuild.
fn is_heavy_scan(report: &crate::indexer::CrawlReport) -> bool {
    if report.walked == 0 || report.reason.starts_with("content index rebuilt") {
        return false;
    }
    let unexplained = report.reindexed.saturating_sub(report.new_files);
    unexplained > 0 && unexplained * 2 > report.walked
}

fn check_semantic_index(units: usize) -> DoctorCheck {
    const ID: &str = "semantic_index";
    const LABEL: &str = "Semantic index";

    let model = crate::embed::ModelRegistry::global();
    if !model.is_enabled() {
        return ok(
            ID,
            LABEL,
            "Semantic indexing disabled (provider: none).".to_string(),
        );
    }

    let detail = if let Some(ref fallback) = model.fallback_reason {
        format!(
            "provider: {} · model_id: {} · dim: {} · {} units (fallback: {})",
            model.provider_name(),
            model.model_id,
            model.dim,
            units,
            fallback
        )
    } else {
        format!(
            "provider: {} · model_id: {} · dim: {} · {} units",
            model.provider_name(),
            model.model_id,
            model.dim,
            units
        )
    };

    // A fallback means the vectors are meaningless even though everything
    // "works": the count is high, queries return results, and the ranking is
    // noise. Reporting ok here is how a user concludes semantic search is fine
    // while it is quietly useless.
    if model.fallback_reason.is_some() {
        return warn(
            ID,
            LABEL,
            detail,
            Some("Download the model files, then rebuild the semantic index.".to_string()),
        );
    }

    if units > 0 {
        ok(ID, LABEL, detail)
    } else {
        warn(
            ID,
            LABEL,
            format!("{} · no units indexed yet", detail),
            Some("Run >reindex once model files are available.".to_string()),
        )
    }
}

fn check_watched_folders(state: &AppState) -> DoctorCheck {
    const ID: &str = "watched_folders";
    const LABEL: &str = "Watched folders";

    let roots = state.indexer.config.read().watched_dirs.clone();

    if roots.is_empty() {
        return warn(
            ID,
            LABEL,
            "No watched folders configured.".to_string(),
            Some("Add at least one folder in setup or Settings.".to_string()),
        );
    }

    let mut missing: Vec<String> = Vec::new();
    for root in &roots {
        let readable = root.is_dir()
            && std::fs::read_dir(root)
                .map(|mut entries| {
                    let _ = entries.next();
                    true
                })
                .unwrap_or(false);
        if !readable {
            missing.push(root.display().to_string());
        }
    }

    if missing.is_empty() {
        return ok(ID, LABEL, format!("{} root(s) readable.", roots.len()));
    }

    let shown = missing
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if missing.len() > 3 { ", ..." } else { "" };
    let detail = format!(
        "{} of {} root(s) missing or unreadable: {}{}",
        missing.len(),
        roots.len(),
        shown,
        suffix
    );

    warn(
        ID,
        LABEL,
        detail,
        Some("Remove missing folders or reconnect the drive in Settings.".to_string()),
    )
}

fn check_watcher(state: &AppState) -> DoctorCheck {
    const ID: &str = "watcher";
    const LABEL: &str = "File watcher";

    if state.indexer.watcher_ok() {
        return ok(ID, LABEL, "Live filesystem watcher is running.".to_string());
    }

    // A crawl stops the watcher for its duration and restarts it at the end,
    // so this state is expected while a scan runs. Reporting it as a warning
    // meant every Health check taken during the launch scan looked broken, and
    // the suggested fix (reindex) would restart what was about to start anyway.
    if state.indexer.is_crawling() {
        return ok(
            ID,
            LABEL,
            "Starts when the current scan finishes.".to_string(),
        );
    }

    with_action(
        warn(
            ID,
            LABEL,
            "Watcher is not running and no scan is in progress.".to_string(),
            None,
        ),
        DoctorAction::new("restart-watcher", "Restart indexing"),
    )
}

#[cfg(target_os = "linux")]
fn check_inotify_limit() -> DoctorCheck {
    const ID: &str = "inotify";
    const LABEL: &str = "Linux inotify limit";
    const FIX: &str = "raise fs.inotify.max_user_watches";
    const MINIMUM: u64 = 100_000;

    let watches = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok());

    match watches {
        Some(value) if value >= MINIMUM => ok(ID, LABEL, format!("max_user_watches = {}", value)),
        Some(value) => warn(
            ID,
            LABEL,
            format!("max_user_watches = {} (below {})", value, MINIMUM),
            Some(FIX.to_string()),
        ),
        None => warn(
            ID,
            LABEL,
            "Could not read /proc/sys/fs/inotify/max_user_watches.".to_string(),
            Some(FIX.to_string()),
        ),
    }
}

fn check_models() -> DoctorCheck {
    const ID: &str = "models";
    const LABEL: &str = "Model files";

    let status = models::status();
    let present = status.artifacts.iter().filter(|a| a.present).count();
    let total = status.artifacts.len();

    if status.ready {
        // Presence is not proof: files that are present but unreadable leave
        // the provider on the hashing fallback, and this row used to report ok
        // anyway while the semantic row reported a fallback. Verify the load.
        let dir = models::models_dir();
        let config = crate::config::KensakuConfig::load();
        match crate::embed::ModelRegistry::load_with_f32_reason_from_dir(
            &dir,
            config.semantic_f32_table,
        ) {
            Ok(_) => ok(
                ID,
                LABEL,
                format!(
                    "{}/{} artifacts present ({}) and loadable",
                    present,
                    total,
                    format_bytes(status.total_present_bytes)
                ),
            ),
            Err(reason) => with_action(
                warn(
                    ID,
                    LABEL,
                    format!(
                        "{}/{} artifacts present ({}), but they cannot be loaded: {}",
                        present,
                        total,
                        format_bytes(status.total_present_bytes),
                        reason
                    ),
                    None,
                ),
                DoctorAction::new("download-models", "Download model files again"),
            ),
        }
    } else {
        let missing = status
            .artifacts
            .iter()
            .filter(|a| !a.present)
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        // Semantic search falls back to hashing when the real model is absent,
        // which returns plausible-looking results ranked on worthless vectors.
        // The row offers the download instead of telling the user to find a
        // button that does not exist in the AI & Semantic panel.
        with_action(
            warn(
                ID,
                LABEL,
                format!(
                    "Missing {} of {} artifacts: {}. Semantic search is using the hashing fallback until they are downloaded.",
                    total - present,
                    total,
                    missing
                ),
                None,
            ),
            DoctorAction::new("download-models", "Download model files"),
        )
    }
}

fn check_ocr() -> DoctorCheck {
    const ID: &str = "ocr";
    const LABEL: &str = "OCR engine";

    if !crate::indexer::extract::ocr_enabled() {
        return ok(ID, LABEL, "OCR is turned off in Settings".to_string());
    }
    let status = crate::indexer::extract::image::ocr_status();
    // The selected engine cannot run: say so instead of silently using
    // another one.
    let selected_problem = match status.engine.as_str() {
        crate::ocr::INDRA => status.indra_problem.clone().map(|p| ("Indra OCR", p)),
        crate::ocr::ACCURATE => status.accurate_problem.clone().map(|p| ("PP-OCR", p)),
        _ => None,
    };
    let fallback = |detail: String| match &selected_problem {
        Some((name, problem)) if status.active_engine != status.engine => warn(
            ID,
            LABEL,
            format!("{name} is selected but cannot run: {problem}. {detail}"),
            None,
        ),
        _ => ok(ID, LABEL, detail),
    };
    match status.active_engine.as_str() {
        crate::ocr::INDRA => ok(
            ID,
            LABEL,
            "Indra OCR is reading images (self-test passed)".to_string(),
        ),
        crate::ocr::ACCURATE => fallback("PP-OCR is reading images (self-test passed)".to_string()),
        crate::ocr::TESSERACT => fallback(format!(
            "Tesseract is reading images · languages: {}",
            status.languages.join(", ")
        )),
        _ => {
            let tesseract = if status.engine_present {
                "Tesseract has no language data".to_string()
            } else {
                match status.error {
                    Some(error) => format!("Tesseract not found ({error})"),
                    None => "Tesseract not found".to_string(),
                }
            };
            let accurate = status
                .accurate_problem
                .unwrap_or_else(|| "PP-OCR cannot run".to_string());
            warn(
                ID,
                LABEL,
                format!("No OCR engine can read images. {tesseract}; {accurate}."),
                Some(
                    "Download the PP-OCR pack in Settings → Search, or install Tesseract"
                        .to_string(),
                ),
            )
        }
    }
}

fn check_ocr_pack() -> DoctorCheck {
    const ID: &str = "ocr_pack";
    if crate::ocr::engine_name() == crate::ocr::INDRA {
        const LABEL: &str = "Indra OCR pack";
        let status = models::indra_status();
        let present = status.artifacts.iter().filter(|a| a.present).count();
        let total = status.artifacts.len();
        if status.ready {
            match crate::ocr_indra::problem() {
                None => ok(
                    ID,
                    LABEL,
                    format!(
                        "installed ({}) and verified on a test image",
                        format_bytes(status.total_present_bytes)
                    ),
                ),
                Some(problem) => warn(ID, LABEL, format!("installed, but {problem}"), None),
            }
        } else {
            with_action(
                warn(
                    ID,
                    LABEL,
                    format!(
                        "Indra OCR is selected but {} of {} artifacts are missing.",
                        total - present,
                        total
                    ),
                    None,
                ),
                DoctorAction::new("download-indra-pack", "Download Indra OCR pack"),
            )
        }
    } else if crate::ocr::engine_name() == crate::ocr::ACCURATE {
        const LABEL: &str = "Accurate PP-OCR pack";
        let status = models::ocr_status();
        let present = status.artifacts.iter().filter(|a| a.present).count();
        let total = status.artifacts.len();
        if status.ready {
            match crate::ocr::accurate_problem() {
                None => ok(
                    ID,
                    LABEL,
                    format!(
                        "installed ({}) and verified on a test image",
                        format_bytes(status.total_present_bytes)
                    ),
                ),
                Some(problem) => warn(ID, LABEL, format!("installed, but {problem}"), None),
            }
        } else {
            with_action(
                warn(
                    ID,
                    LABEL,
                    format!(
                        "Accurate PP-OCR is selected but {} of {} artifacts are missing.",
                        total - present,
                        total
                    ),
                    None,
                ),
                DoctorAction::new("download-ocr-pack", "Download PP-OCR pack"),
            )
        }
    } else {
        ok(
            ID,
            "OCR pack",
            "Tesseract engine is selected (no ONNX pack needed).".to_string(),
        )
    }
}

fn check_version() -> DoctorCheck {
    ok(
        "version",
        "Kensaku version",
        env!("CARGO_PKG_VERSION").to_string(),
    )
}

fn check_external_extractors(
    extractors: &[ExternalExtractor],
    failures: &[(std::path::PathBuf, String)],
) -> DoctorCheck {
    const ID: &str = "external_extractors";
    const LABEL: &str = "External extractors";

    if extractors.is_empty() {
        return ok(
            ID,
            LABEL,
            "No external extractors configured (none ship by default).".to_string(),
        );
    }

    let commands = extractors
        .iter()
        .map(|extractor| {
            let command = if extractor.command.chars().count() > 160 {
                let mut short: String = extractor.command.chars().take(157).collect();
                short.push_str("...");
                short
            } else {
                extractor.command.clone()
            };
            format!(
                "{} → {} ({})",
                extractor.name.trim(),
                command,
                if extractor.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");

    let invalid = extractors
        .iter()
        .filter_map(|extractor| {
            extractor
                .validate()
                .err()
                .map(|error| format!("{}: {}", extractor.name.trim(), error))
        })
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        return fail(
            ID,
            LABEL,
            format!("Invalid configuration: {}", invalid.join("; ")),
            Some(
                "Fix the command, selectors or executable permissions in config.json.".to_string(),
            ),
        );
    }

    let enabled = extractors
        .iter()
        .filter(|extractor| extractor.enabled)
        .count();
    let failure_detail = if failures.is_empty() {
        String::new()
    } else {
        let shown = failures
            .iter()
            .rev()
            .take(3)
            .map(|(path, reason)| {
                let reason = if reason.chars().count() > 140 {
                    let mut short: String = reason.chars().take(137).collect();
                    short.push_str("...");
                    short
                } else {
                    reason.clone()
                };
                format!("{} — {}", path.display(), reason)
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(" · {} recent failure(s): {}", failures.len(), shown)
    };
    if enabled == 0 {
        return warn(
            ID,
            LABEL,
            format!(
                "{} configured but none enabled: {}",
                extractors.len(),
                commands
            ),
            Some("Review each command, then set enabled: true in config.json.".to_string()),
        );
    }

    if !failure_detail.is_empty() {
        return warn(
            ID,
            LABEL,
            format!("{} enabled: {}{}", enabled, commands, failure_detail),
            Some(
                "Fix the plugin, then save its config or run a full reindex to retry.".to_string(),
            ),
        );
    }

    ok(ID, LABEL, format!("{} enabled: {}", enabled, commands))
}

/// Runs every doctor check. Never panics; each check degrades to a status.
pub fn run(state: &AppState) -> DoctorReport {
    let config_dir = KensakuConfig::config_dir();

    let nodes = state.indexer.metadata.read().len();
    let units = state.indexer.unit_store.read().len();

    let mut checks = vec![
        check_config_dir_writable(),
        check_storage(),
        check_disk_space(&config_dir),
        check_crash_log(),
        check_metadata_db(&config_dir),
        check_full_text_index(nodes),
        check_last_crawl(),
        check_semantic_index(units),
        check_watched_folders(state),
        check_watcher(state),
        check_external_extractors(
            &state.indexer.config.read().extractors,
            &external::recent_failures(),
        ),
    ];
    #[cfg(target_os = "linux")]
    checks.push(check_inotify_limit());
    checks.push(check_models());
    checks.push(check_ocr());
    checks.push(check_ocr_pack());
    checks.push(check_version());

    let mut summary = DoctorSummary {
        ok: 0,
        warn: 0,
        fail: 0,
    };
    for c in &checks {
        match c.status.as_str() {
            "ok" => summary.ok += 1,
            "warn" => summary.warn += 1,
            _ => summary.fail += 1,
        }
    }

    DoctorReport {
        checks,
        ran_at: now_unix_secs(),
        summary,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::indexer::CrawlReport;

    fn extractor(command: &str, enabled: bool) -> ExternalExtractor {
        ExternalExtractor {
            name: "sample".to_string(),
            extensions: vec!["k10ext".to_string()],
            mime: Vec::new(),
            command: command.to_string(),
            args: Vec::new(),
            timeout_ms: 1_000,
            max_output_bytes: 1_000,
            enabled,
        }
    }

    #[test]
    fn external_check_is_ok_when_nothing_is_configured() {
        let check = check_external_extractors(&[], &[]);
        assert_eq!(check.status, "ok");
        assert!(check.detail.contains("none ship by default"));
    }

    #[test]
    fn external_check_shows_the_command_while_disabled() {
        let check = check_external_extractors(&[extractor("/bin/cat", false)], &[]);
        assert_eq!(check.status, "warn");
        assert!(check.detail.contains("/bin/cat"), "got: {}", check.detail);
    }

    #[test]
    fn external_check_fails_broken_enabled_commands() {
        let check =
            check_external_extractors(&[extractor("/definitely/missing/plugin", true)], &[]);
        assert_eq!(check.status, "fail");
        assert!(check.fix.is_some());
    }

    #[test]
    fn external_check_accepts_a_reviewed_enabled_command() {
        let check = check_external_extractors(&[extractor("/bin/cat", true)], &[]);
        assert_eq!(check.status, "ok");
    }

    #[test]
    fn external_check_surfaces_runtime_failures() {
        let failures = vec![(
            std::path::PathBuf::from("/docs/model.k10ext"),
            "timed out after 200 ms".to_string(),
        )];
        let check = check_external_extractors(&[extractor("/bin/cat", true)], &failures);
        assert_eq!(check.status, "warn");
        assert!(check.detail.contains("model.k10ext"));
        assert!(check.detail.contains("timed out"));
    }

    #[test]
    fn disk_space_check_reports_a_status() {
        let dir = std::env::temp_dir();
        let check = check_disk_space(&dir);
        assert!(
            matches!(check.status.as_str(), "ok" | "warn" | "fail"),
            "unexpected status: {}",
            check.status
        );
        assert_eq!(check.id, "disk_space");
    }

    fn crawl(walked: usize, reindexed: usize, new_files: usize, reason: &str) -> CrawlReport {
        CrawlReport {
            walked,
            reindexed,
            new_files,
            reason: reason.to_string(),
            ..CrawlReport::default()
        }
    }

    #[test]
    fn first_scan_is_not_a_heavy_scan() {
        // #142 / #146: every file is new, so the whole corpus is re-read by
        // definition and the user must not be told to report a bug.
        let report = crawl(
            88_391,
            52_257,
            87_387,
            "0 left pending by an earlier scan, 54 retried, 0 given up on; 87387 new, 0 with a different size or modification time",
        );
        assert!(!is_heavy_scan(&report));
    }

    #[test]
    fn recreated_index_is_not_a_heavy_scan() {
        let report = crawl(63_661, 37_141, 63_086, "63086 new, 0 restamped");
        assert!(!is_heavy_scan(&report));
    }

    #[test]
    fn churn_over_known_files_is_a_heavy_scan() {
        // Nothing new, yet most of the corpus was re-read: the pattern worth
        // reporting.
        let report = crawl(10_000, 9_000, 0, "0 new, 0 restamped");
        assert!(is_heavy_scan(&report));
    }

    #[test]
    fn a_small_re_read_is_not_a_heavy_scan() {
        let report = crawl(10_000, 500, 0, "0 new, 500 restamped");
        assert!(!is_heavy_scan(&report));
    }

    #[test]
    fn a_content_index_rebuild_explains_the_re_read() {
        let report = crawl(
            10_000,
            10_000,
            0,
            "content index rebuilt: embedding model changed",
        );
        assert!(!is_heavy_scan(&report));
    }

    #[test]
    fn a_mixed_scan_only_counts_the_unexplained_re_read() {
        // 9,000 re-read, 8,500 of them new: 500 unexplained files out of
        // 10,000 is ordinary churn, not a heavy scan.
        let report = crawl(10_000, 9_000, 8_500, "8500 new, 0 restamped");
        assert!(!is_heavy_scan(&report));
    }

    #[test]
    fn an_empty_walk_is_not_a_heavy_scan() {
        assert!(!is_heavy_scan(&crawl(0, 0, 0, "nothing changed")));
    }
}
