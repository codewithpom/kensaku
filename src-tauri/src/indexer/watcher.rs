use crate::config::KensakuConfig;
use crate::indexer::content::{
    ContentIndex, is_indexable_extension, is_indexable_file_name, read_text_content,
};
use crate::indexer::extract::image as image_util;
use crate::indexer::metadata::MetadataStore;
use crate::indexer::walker::{
    FileEntry, file_identity, is_cloud_placeholder, is_cloud_placeholder_path, is_sensitive_path,
    prunes_for_roots, starts_with_any, walk_directories,
};
use notify::event::{ModifyKind, RenameMode};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Handle to a running filesystem watcher thread.
///
/// Dropping the handle signals the thread to stop and waits for it to exit.
/// This keeps repeated `start_indexing` calls from leaving duplicate watchers
/// alive.
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl WatcherHandle {
    /// Signal the watcher to stop and wait for its thread to finish.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How long a path must be quiet before the watcher reads it.
const SETTLE: Duration = Duration::from_millis(400);
/// Overflow events can arrive in storms: on Linux an inotify queue overflow
/// over a large or network-backed tree reports `need_rescan`, and the signal
/// repeats while the tree keeps changing. A rescan walks every root, so
/// coalesce them instead of starting one per event.
const RESCAN_COOLDOWN: Duration = Duration::from_secs(60);
/// Extra observation for files whose mtime is still inside the retry window.
const FRESH_RETRY: Duration = Duration::from_millis(500);
/// A file written this recently is re-checked before its content is trusted.
const FRESH_WINDOW: Duration = Duration::from_secs(10);

struct PendingUpsert {
    deadline: Instant,
    first_seen: Instant,
    /// `(size, mtime_secs)` from the previous settle attempt. A changing
    /// fingerprint means the writer is still active, so keep waiting.
    fingerprint: Option<(u64, u64)>,
}

/// Coalesces create/modify events per path and delays them until they settle.
#[derive(Default)]
struct PendingUpserts {
    paths: HashMap<PathBuf, PendingUpsert>,
}

impl PendingUpserts {
    fn note(&mut self, path: PathBuf, now: Instant) {
        self.paths
            .entry(path)
            .and_modify(|pending| pending.deadline = now + SETTLE)
            .or_insert(PendingUpsert {
                deadline: now + SETTLE,
                first_seen: now,
                fingerprint: None,
            });
    }

    fn due(&mut self, now: Instant) -> Vec<(PathBuf, PendingUpsert)> {
        let ready: Vec<PathBuf> = self
            .paths
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(path, _)| path.clone())
            .collect();
        ready
            .into_iter()
            .filter_map(|path| self.paths.remove(&path).map(|pending| (path, pending)))
            .collect()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.paths.values().map(|pending| pending.deadline).min()
    }

    fn len(&self) -> usize {
        self.paths.len()
    }
}

fn modified_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Applies a settled path, re-queueing fresh files until their size and mtime
/// stop moving (browser saves often create then keep writing).
fn apply_settled(
    path: PathBuf,
    pending: PendingUpsert,
    now: Instant,
    cfg: &KensakuConfig,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
    queue: &mut PendingUpserts,
) {
    let Ok(meta) = std::fs::metadata(&path) else {
        handle_path_remove(&path, metadata, content_index);
        return;
    };

    let fingerprint = (meta.len(), modified_secs(&meta));
    let fresh = now
        .checked_duration_since(pending.first_seen)
        .is_some_and(|elapsed| elapsed < FRESH_WINDOW)
        && SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .map(|now| now.as_secs().saturating_sub(fingerprint.1) < FRESH_WINDOW.as_secs())
            .unwrap_or(false);

    if fresh && meta.is_file() && pending.fingerprint != Some(fingerprint) {
        queue.paths.insert(
            path,
            PendingUpsert {
                deadline: now + FRESH_RETRY,
                first_seen: pending.first_seen,
                fingerprint: Some(fingerprint),
            },
        );
        return;
    }

    if is_path_excluded(&path, cfg) || !path.exists() {
        if !path.exists() {
            handle_path_remove(&path, metadata, content_index);
        }
        return;
    }

    handle_file_upsert(&path, metadata, content_index);
}

pub fn start_file_watcher(
    watched_dirs: Vec<PathBuf>,
    metadata: Arc<RwLock<MetadataStore>>,
    content_index: Arc<RwLock<Option<ContentIndex>>>,
    config: Arc<RwLock<KensakuConfig>>,
    watcher_ok: Arc<AtomicBool>,
) -> WatcherHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    watcher_ok.store(false, Ordering::SeqCst);

    let thread = thread::spawn(move || {
        run_watcher(
            watched_dirs,
            metadata,
            content_index,
            config,
            stop_thread,
            watcher_ok,
        );
    });

    WatcherHandle {
        stop,
        thread: Some(thread),
    }
}

fn run_watcher(
    watched_dirs: Vec<PathBuf>,
    metadata: Arc<RwLock<MetadataStore>>,
    content_index: Arc<RwLock<Option<ContentIndex>>>,
    config: Arc<RwLock<KensakuConfig>>,
    stop: Arc<AtomicBool>,
    watcher_ok: Arc<AtomicBool>,
) {
    let (tx, rx) = channel::<notify::Result<Event>>();

    let mut watcher = match RecommendedWatcher::new(tx, Config::default()) {
        Ok(w) => w,
        Err(e) => {
            crate::say_err!("[Kensaku Watcher] Failed to initialize watcher: {}", e);
            watcher_ok.store(false, Ordering::SeqCst);
            return;
        }
    };

    // Report live indexing only once a watch is actually registered. The flag
    // used to be set before the loop, so an exhausted inotify limit, a
    // network mount or a denied FSEvents subscription still looked healthy
    // while nothing was being watched.
    let mut registered = 0usize;
    for root in &watched_dirs {
        if !root.exists() {
            continue;
        }
        match watcher.watch(root, RecursiveMode::Recursive) {
            Ok(()) => {
                registered += 1;
                crate::say!("[Kensaku Watcher] Watching {}", root.display());
            }
            Err(error) => {
                crate::say_err!(
                    "[Kensaku Watcher] Failed to watch {}: {}",
                    root.display(),
                    error
                );
            }
        }
    }
    watcher_ok.store(registered > 0, Ordering::SeqCst);

    let mut pending = PendingUpserts::default();
    let mut pending_commits = 0;
    let mut last_commit = SystemTime::now();
    let mut last_rescan: Option<Instant> = None;
    let rescan_running = Arc::new(AtomicBool::new(false));

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let cfg = config.read().clone();
        let now = Instant::now();

        // Apply paths that have been quiet for the settle window. Pausing only
        // gates reads/indexing; removes and renames are still applied below.
        if !cfg.pause_indexing {
            for (path, entry) in pending.due(now) {
                apply_settled(
                    path,
                    entry,
                    now,
                    &cfg,
                    &metadata,
                    &content_index,
                    &mut pending,
                );
                pending_commits += 1;
            }
        }

        // Drain all available events in a batch (coalescing) instead of
        // one-at-a-time. Read config once per batch, not per event.
        let mut batch_count = 0;
        loop {
            match rx.try_recv() {
                Ok(Ok(event)) => {
                    pending_commits += handle_event(
                        &event,
                        &cfg,
                        &metadata,
                        &content_index,
                        &mut pending,
                        Instant::now(),
                    );
                    maybe_spawn_rescan(
                        event.need_rescan(),
                        &mut last_rescan,
                        &watched_dirs,
                        &cfg,
                        &metadata,
                        &content_index,
                        &rescan_running,
                    );
                    batch_count += 1;
                    if batch_count >= 100 {
                        break; // Process max 100 per batch to avoid blocking
                    }
                }
                Ok(Err(e)) => {
                    crate::say_err!("[Kensaku Watcher] Error event: {:?}", e);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // No more events; fall through to commit check
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return; // Channel closed
                }
            }
        }

        // If no events were drained, wait until the next settle deadline (or a
        // short timeout so pause/unpause and shutdown stay responsive).
        if batch_count == 0 {
            let wait = if cfg.pause_indexing {
                Duration::from_millis(500)
            } else {
                pending
                    .next_deadline()
                    .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_millis(500))
                    .clamp(Duration::from_millis(50), Duration::from_millis(500))
            };
            match rx.recv_timeout(wait) {
                Ok(Ok(event)) => {
                    pending_commits += handle_event(
                        &event,
                        &cfg,
                        &metadata,
                        &content_index,
                        &mut pending,
                        Instant::now(),
                    );
                    maybe_spawn_rescan(
                        event.need_rescan(),
                        &mut last_rescan,
                        &watched_dirs,
                        &cfg,
                        &metadata,
                        &content_index,
                        &rescan_running,
                    );
                }
                Ok(Err(e)) => {
                    crate::say_err!("[Kensaku Watcher] Error event: {:?}", e);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }

        // Periodic Tantivy commit if needed
        if pending_commits > 0
            && let Ok(elapsed) = last_commit.elapsed()
            && (elapsed >= Duration::from_secs(3)
                || pending_commits > 50
                || (pending.len() == 0 && pending_commits > 0))
        {
            if let Some(c_index) = content_index.read().as_ref() {
                let _ = c_index.commit();
            }
            pending_commits = 0;
            last_commit = SystemTime::now();
        }
    }

    // Flush anything still pending before the thread exits.
    if pending_commits > 0
        && let Some(c_index) = content_index.read().as_ref()
    {
        let _ = c_index.commit();
    }
}

/// True when another overflow rescan is allowed at `now`.
fn rescan_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= RESCAN_COOLDOWN)
}

/// Starts an overflow rescan when the cooldown allows it and nothing is
/// already running. Returns whether a rescan was actually started.
fn maybe_spawn_rescan(
    need_rescan: bool,
    last_rescan: &mut Option<Instant>,
    watched_dirs: &[PathBuf],
    cfg: &KensakuConfig,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
    running: &Arc<AtomicBool>,
) {
    let now = Instant::now();
    if !need_rescan || !rescan_due(*last_rescan, now) {
        return;
    }
    if spawn_rescan(watched_dirs, cfg, metadata, content_index, running) {
        *last_rescan = Some(now);
    }
}

/// Reconciles the whole watched tree after a platform overflow/rescan signal.
///
/// Returns `false` when a rescan is already in flight; the running pass is
/// walking the same tree, so overlapping is never useful.
fn spawn_rescan(
    watched_dirs: &[PathBuf],
    cfg: &KensakuConfig,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
    running: &Arc<AtomicBool>,
) -> bool {
    if running.swap(true, Ordering::SeqCst) {
        return false;
    }
    let dirs = watched_dirs.to_vec();
    let cfg = cfg.clone();
    let metadata = Arc::clone(metadata);
    let content_index = Arc::clone(content_index);
    let running = Arc::clone(running);
    thread::spawn(move || {
        let changed = rescan_watched_dirs(&dirs, &cfg, &metadata, &content_index);
        if changed > 0 {
            crate::say_err!("[Kensaku Watcher] Rescan reconciled {} paths", changed);
        }
        running.store(false, Ordering::SeqCst);
    });
    true
}

fn rescan_watched_dirs(
    watched_dirs: &[PathBuf],
    cfg: &KensakuConfig,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
) -> usize {
    let entries = walk_directories(
        watched_dirs,
        &cfg.excluded_dirs,
        cfg.include_ignored,
        cfg.skip_hidden,
    );
    let seen: HashSet<PathBuf> = entries
        .iter()
        .filter(|entry| !is_path_excluded(&entry.path, cfg))
        .map(|entry| entry.path.clone())
        .collect();

    let mut changed = 0;
    for entry in &entries {
        if is_path_excluded(&entry.path, cfg) {
            continue;
        }
        // A rescan exists to reconcile paths the watcher may have missed, not
        // to re-read the tree. Skipping unchanged entries is what keeps an
        // overflow storm from turning into a full-corpus re-index (large
        // corpora on mounted drives overflow inotify regularly). Pending
        // entries are still retried, and a changed file always differs in at
        // least one of mtime/size.
        let unchanged = metadata
            .read()
            .get(&entry.path)
            .is_some_and(|existing| super::is_unchanged(&existing, entry));
        if unchanged {
            continue;
        }
        handle_file_upsert(&entry.path, metadata, content_index);
        changed += 1;
    }

    // Only roots that can be read can prove a path is gone; unmounted drives
    // keep their metadata.
    let available: Vec<&PathBuf> = watched_dirs
        .iter()
        .filter(|root| std::fs::read_dir(root).is_ok())
        .collect();
    let stale: Vec<PathBuf> = {
        let store = metadata.read();
        (0..store.len())
            .filter_map(|index| {
                let path = store.path(index as u32);
                if available.iter().any(|root| path.starts_with(root)) && !seen.contains(&path) {
                    Some(path)
                } else {
                    None
                }
            })
            .collect()
    };
    for path in &stale {
        handle_path_remove(path, metadata, content_index);
        changed += 1;
    }

    if let Some(c_index) = content_index.read().as_ref() {
        let _ = c_index.commit();
    }
    changed
}

fn is_path_excluded(path: &Path, cfg: &KensakuConfig) -> bool {
    // Kensaku's own runtime data (index, SQLite WAL, thumbnails, OCR packs)
    // lives under `config_dir()` and is written while the watcher runs. On
    // macOS that path is not hidden, so every write would otherwise be
    // re-indexed through these very events. Pruned by path, matching the
    // crawler's `runtime_dir_prunes`.
    if starts_with_any(path, &prunes_for_roots(&cfg.watched_dirs)) {
        return true;
    }
    // Honour the ignore files the crawler used, so the watcher and the crawl
    // agree about what belongs in the index.
    if crate::indexer::ignore::is_active_ignored(path, path.is_dir()) {
        return true;
    }
    // The crawl drops excluded extensions and sensitive names before adding
    // anything. The watcher used to skip both checks, so saving an excluded
    // or secret-bearing file put it straight back into the index until the
    // next full crawl.
    if !path.is_dir() && cfg.is_extension_excluded(path) {
        return true;
    }
    if is_sensitive_path(path) {
        return true;
    }
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|name| {
                // Match the crawler's hidden-entry setting.
                cfg.skip_hidden && name.starts_with('.') || cfg.is_dir_excluded(name)
            })
            .unwrap_or(false)
    })
}

/// Applies a single watcher event, returning how many index mutations occurred.
fn handle_event(
    event: &Event,
    cfg: &KensakuConfig,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
    pending: &mut PendingUpserts,
    now: Instant,
) -> usize {
    let mut changes = 0;

    match &event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // `paths` is ordered (from, to) for this event kind.
            if event.paths.len() >= 2 {
                let old_path = &event.paths[0];
                let new_path = &event.paths[1];

                let new_excluded = is_path_excluded(new_path, cfg);

                if new_path.is_dir() {
                    // Moving a directory does not produce per-child events, so
                    // the whole subtree has to be translated to the new prefix
                    // (and its content re-indexed) instead of being dropped.
                    if !new_excluded {
                        handle_directory_rename(old_path, new_path, cfg, metadata, content_index);
                        changes += 1;
                    } else {
                        // Moved into an excluded location: drop the subtree.
                        // Removal runs even when the old path is excluded too,
                        // so tightening the policy cleans up stale rows.
                        handle_path_remove(old_path, metadata, content_index);
                        changes += 1;
                    }
                } else {
                    // Drop the old row unconditionally: an excluded old path
                    // may still have stale metadata from before the policy
                    // changed.
                    handle_path_remove(old_path, metadata, content_index);
                    changes += 1;
                    if !new_excluded {
                        pending.note(new_path.clone(), now);
                        changes += 1;
                    }
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            // Removes are safe during a pause: they only drop stale rows.
            // Apply them even for excluded paths so a tightened policy can
            // clean up rows written before the change.
            for path in &event.paths {
                handle_path_remove(path, metadata, content_index);
                changes += 1;
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            for path in &event.paths {
                if !is_path_excluded(path, cfg) {
                    pending.note(path.clone(), now);
                    changes += 1;
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(_)) => {
            // Rename direction unknown: reconcile using the current filesystem state.
            for path in &event.paths {
                if !path.exists() || is_path_excluded(path, cfg) {
                    handle_path_remove(path, metadata, content_index);
                } else {
                    pending.note(path.clone(), now);
                }
                changes += 1;
            }
        }
        EventKind::Remove(_) => {
            // Always remove, even for excluded paths: that is how a policy
            // change (or a file that became sensitive) drops stale rows.
            for path in &event.paths {
                handle_path_remove(path, metadata, content_index);
                changes += 1;
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) => {
            for path in &event.paths {
                if !is_path_excluded(path, cfg) {
                    pending.note(path.clone(), now);
                    changes += 1;
                }
            }
        }
        _ => {}
    }

    changes
}

/// Recomputes screenshot-burst membership for one image after its perceptual
/// hash and extracted text are known. Grouping is best-effort and never blocks
/// indexing.
fn assign_image_burst(path: &Path, content: Option<&str>, metadata: &Arc<RwLock<MetadataStore>>) {
    let Some(db) = metadata.read().db() else {
        return;
    };
    let signature = content
        .map(crate::indexer::burst::line_signature)
        .unwrap_or_default();
    let Ok(updates) = db.set_image_text_sig(path, &signature) else {
        metadata.write().clear_burst_for_path(path);
        return;
    };
    if !updates.is_empty() {
        metadata.write().apply_bursts(&updates);
    }
    match db.assign_burst_for_path(path) {
        Ok(updates) if !updates.is_empty() => metadata.write().apply_bursts(&updates),
        _ => metadata.write().clear_burst_for_path(path),
    }
}

/// Applies a directory rename that the watcher reported as a single event.
///
/// Rewrites the stored subtree under the metadata write lock (one database
/// transaction), drops the content documents that belonged to the old paths,
/// and then walks the new directory in a short-lived thread to restore content
/// without waiting for a restart.
fn handle_directory_rename(
    old_path: &Path,
    new_path: &Path,
    cfg: &KensakuConfig,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
) {
    let old_paths = metadata.write().rename_prefix(old_path, new_path);

    // The old path terms no longer match anything; the repair walk re-adds
    // fresh documents under the new paths.
    if let Some(c_index) = content_index.read().as_ref() {
        for path in &old_paths {
            let _ = c_index.remove(path);
        }
        let _ = c_index.commit();
    }

    let new_dir = new_path.to_path_buf();
    let excluded_dirs = cfg.excluded_dirs.clone();
    let excluded_extensions = cfg.excluded_extensions.clone();
    let include_ignored = cfg.include_ignored;
    let skip_hidden = cfg.skip_hidden;
    let metadata = Arc::clone(metadata);
    let content_index = Arc::clone(content_index);

    // A large directory move can take a while; re-index it off the watcher
    // thread so event draining is not blocked.
    thread::spawn(move || {
        let entries = walk_directories(
            std::slice::from_ref(&new_dir),
            &excluded_dirs,
            include_ignored,
            skip_hidden,
        );
        let mut pending_commits = 0usize;

        for entry in entries {
            // The crawl drops excluded extensions before touching metadata;
            // a rename must do the same or moving a folder re-adds them.
            if !entry.is_dir
                && entry.extension.is_some_and(|extension| {
                    excluded_extensions.iter().any(|excluded| {
                        excluded
                            .trim_start_matches('.')
                            .eq_ignore_ascii_case(extension)
                    })
                })
            {
                continue;
            }
            let is_indexable = !entry.is_dir
                && (is_indexable_extension(entry.extension)
                    || is_indexable_file_name(&entry.name)
                    || crate::indexer::extract::external::handles(&entry.path));
            let path = entry.path.clone();
            let name = entry.name.clone();
            let modified = entry.modified;
            let size = entry.size;
            let extension = entry.extension;
            let is_image = entry
                .extension
                .is_some_and(image_util::is_supported_image_extension);

            metadata.write().add(entry);

            let image_db = metadata.read().db();
            let mut image_hashed = false;
            if is_image
                && !is_cloud_placeholder_path(&path)
                && let (Some((width, height)), Some(phash)) =
                    (image_util::dimensions(&path), image_util::phash64(&path))
                && let Some(db) = image_db.as_ref()
                && let Ok(updates) = db.set_image_meta(&path, width, height, phash)
            {
                image_hashed = true;
                if !updates.is_empty() {
                    metadata.write().apply_bursts(&updates);
                }
            }
            if is_image && !is_cloud_placeholder_path(&path) && !image_hashed {
                if let Some(db) = image_db.as_ref()
                    && let Ok(updates) = db.clear_image_hash(&path)
                    && !updates.is_empty()
                {
                    metadata.write().apply_bursts(&updates);
                }
                metadata.write().clear_burst_for_path(&path);
            }

            // Cloud placeholders keep metadata only: reading them would
            // download the file.
            if !is_indexable || is_cloud_placeholder_path(&path) {
                continue;
            }

            let content = read_text_content(
                &path,
                crate::indexer::extract::content_max_text_bytes() as u64,
            );
            if let Some(content) = content.as_deref()
                && let Some(c_index) = content_index.read().as_ref()
            {
                let _ = c_index.add_or_update(&path, &name, content, modified, size, extension);
                pending_commits += 1;
                if pending_commits >= 1000 {
                    let _ = c_index.commit();
                    pending_commits = 0;
                }
            }
            if is_image {
                assign_image_burst(&path, content.as_deref(), &metadata);
            }
        }

        if let Some(c_index) = content_index.read().as_ref() {
            let _ = c_index.commit();
        }
    });
}

fn handle_file_upsert(
    path: &Path,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
) {
    // Skip entries whose parent directory is already gone: creating them would
    // leave orphan rows behind after a directory removal.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return;
    }

    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return,
    };

    let is_dir = meta.is_dir();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = if is_dir {
        None
    } else {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|s| s.to_lowercase())
    };

    let size = meta.len();
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (dev, ino) = file_identity(&meta);
    let char_bloom = crate::search::fuzzy::compute_char_mask(&name);

    let entry = FileEntry {
        id: 0,
        path: path.to_path_buf(),
        name: name.clone(),
        extension: crate::indexer::walker::intern_extension(extension.as_deref()),
        size,
        modified,
        is_dir,
        dev,
        ino,
        char_bloom,
        content_pending: false,
    };

    metadata.write().add(entry);

    // Image dimensions and perceptual hash are best-effort so duplicate
    // detection stays fresh for changed files. Thumbnails remain lazy and are
    // only generated by the `get_thumbnail` command.
    let is_image = extension
        .as_deref()
        .is_some_and(image_util::is_supported_image_extension);
    let image_db = metadata.read().db();
    let mut image_hashed = false;
    if !is_dir
        && is_image
        && !is_cloud_placeholder(&meta)
        && let (Some((width, height)), Some(phash)) =
            (image_util::dimensions(path), image_util::phash64(path))
        && let Some(db) = image_db.as_ref()
        && let Ok(updates) = db.set_image_meta(path, width, height, phash)
    {
        image_hashed = true;
        if !updates.is_empty() {
            metadata.write().apply_bursts(&updates);
        }
    }
    if !is_dir && is_image && !is_cloud_placeholder(&meta) && !image_hashed {
        if let Some(db) = image_db.as_ref()
            && let Ok(updates) = db.clear_image_hash(path)
            && !updates.is_empty()
        {
            metadata.write().apply_bursts(&updates);
        }
        metadata.write().clear_burst_for_path(path);
    }

    let content = if !is_dir
        && (is_indexable_extension(extension.as_deref())
            || is_indexable_file_name(&name)
            || crate::indexer::extract::external::handles(path))
        && !is_cloud_placeholder(&meta)
    {
        // Match the crawl's configured cap. A hard-coded 100 KB truncated the
        // first save of any larger file, and the content index kept that
        // truncated document until the next full crawl.
        read_text_content(
            path,
            crate::indexer::extract::content_max_text_bytes() as u64,
        )
    } else {
        None
    };
    if let Some(content) = content.as_deref()
        && let Some(c_index) = content_index.read().as_ref()
    {
        let _ = c_index.add_or_update(
            path,
            &name,
            content,
            modified,
            meta.len(),
            extension.as_deref(),
        );
    }

    if !is_dir && is_image && !is_cloud_placeholder(&meta) {
        assign_image_burst(path, content.as_deref(), metadata);
    }
}

/// Removes a path from metadata and the content index.
///
/// When the path was a directory, every indexed descendant is removed as well,
/// since the filesystem no longer reports individual removals for all children.
fn handle_path_remove(
    path: &Path,
    metadata: &Arc<RwLock<MetadataStore>>,
    content_index: &Arc<RwLock<Option<ContentIndex>>>,
) {
    let was_dir = metadata
        .read()
        .get(path)
        .map(|entry| entry.is_dir)
        .unwrap_or(false);

    metadata.write().remove(path);
    if let Some(c_index) = content_index.read().as_ref() {
        let _ = c_index.remove(path);
    }

    if !was_dir {
        return;
    }

    let descendants: Vec<PathBuf> = {
        let store = metadata.read();
        (0..store.len())
            .filter_map(|i| {
                let e = store.materialize(i as u32);
                if e.path != path && e.path.starts_with(path) {
                    Some(e.path)
                } else {
                    None
                }
            })
            .collect()
    };

    if descendants.is_empty() {
        return;
    }

    {
        let mut store = metadata.write();
        for descendant in &descendants {
            store.remove(descendant);
        }
    }

    if let Some(c_index) = content_index.read().as_ref() {
        for descendant in &descendants {
            let _ = c_index.remove(descendant);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::metadata_db::MetadataDb;
    use notify::event::{CreateKind, RemoveKind};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kensaku-watcher-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store(dir: &Path) -> Arc<RwLock<MetadataStore>> {
        let db = MetadataDb::open_in_dir(dir).expect("open metadata db");
        Arc::new(RwLock::new(MetadataStore::with_db(db)))
    }

    fn no_content() -> Arc<RwLock<Option<ContentIndex>>> {
        Arc::new(RwLock::new(None))
    }

    fn event(kind: EventKind, paths: &[&Path]) -> Event {
        let mut event = Event::new(kind);
        for path in paths {
            event = event.add_path(path.to_path_buf());
        }
        event
    }

    fn settle_old(
        path: PathBuf,
        cfg: &KensakuConfig,
        metadata: &Arc<RwLock<MetadataStore>>,
        content_index: &Arc<RwLock<Option<ContentIndex>>>,
    ) {
        let now = Instant::now();
        let pending = PendingUpsert {
            deadline: now,
            first_seen: now - FRESH_WINDOW - Duration::from_secs(1),
            fingerprint: None,
        };
        let mut queue = PendingUpserts::default();
        apply_settled(path, pending, now, cfg, metadata, content_index, &mut queue);
        assert!(queue.len() == 0, "old files settle immediately");
    }

    #[test]
    fn watcher_refuses_excluded_extensions_and_sensitive_names() {
        let dir = temp_dir("excluded");
        let cfg = KensakuConfig {
            watched_dirs: vec![dir.clone()],
            excluded_extensions: vec!["secret".to_string()],
            ..KensakuConfig::default()
        };
        let metadata = store(&dir);
        let content_index = no_content();
        let mut queue = PendingUpserts::default();

        let excluded_ext = dir.join("notes.secret");
        let env_file = dir.join(".env.production");
        let credential_dir = dir.join(".aws");
        std::fs::create_dir_all(&credential_dir).unwrap();
        let credential_file = credential_dir.join("credentials");
        std::fs::write(&excluded_ext, "x").unwrap();
        std::fs::write(&env_file, "TOKEN=1").unwrap();
        std::fs::write(&credential_file, "aws_secret_access_key=1").unwrap();

        let now = Instant::now();
        let changes = handle_event(
            &event(
                EventKind::Create(CreateKind::File),
                &[&excluded_ext, &env_file, &credential_file],
            ),
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
            now,
        );

        assert_eq!(changes, 0, "excluded files must not be queued");
        assert_eq!(queue.len(), 0);
        assert!(is_path_excluded(&excluded_ext, &cfg));
        assert!(is_path_excluded(&env_file, &cfg));
        assert!(is_path_excluded(&credential_file, &cfg));
    }

    #[test]
    fn events_are_coalesced_until_the_settle_deadline() {
        let now = Instant::now();
        let mut queue = PendingUpserts::default();
        queue.note(PathBuf::from("/tmp/a"), now);
        queue.note(PathBuf::from("/tmp/a"), now + Duration::from_millis(100));

        assert_eq!(queue.len(), 1);
        assert!(queue.due(now + Duration::from_millis(499)).is_empty());
        assert_eq!(queue.due(now + Duration::from_millis(501)).len(), 1);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn create_event_waits_for_settle_before_indexing() {
        let dir = temp_dir("settle");
        let file = dir.join("note.txt");
        std::fs::write(&file, "hello").unwrap();
        // Watch the temp root explicitly: on Windows the default watched root
        // (home) prunes `~\AppData`, which contains this test's temp directory
        // and would exclude every event here.
        let cfg = KensakuConfig {
            watched_dirs: vec![dir.clone()],
            ..KensakuConfig::default()
        };
        let metadata = store(&dir);
        let content_index = no_content();
        let mut queue = PendingUpserts::default();

        let now = Instant::now();
        handle_event(
            &event(EventKind::Create(CreateKind::File), &[&file]),
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
            now,
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(metadata.read().len(), 0, "must not read before settle");
    }

    #[test]
    fn fresh_file_is_requeued_until_its_fingerprint_is_stable() {
        let dir = temp_dir("fresh");
        let file = dir.join("download.part");
        std::fs::write(&file, "partial").unwrap();
        // See `create_event_waits_for_settle_before_indexing`: watch the temp
        // root so Windows' under-AppData prune does not swallow every event.
        let cfg = KensakuConfig {
            watched_dirs: vec![dir.clone()],
            ..KensakuConfig::default()
        };
        let metadata = store(&dir);
        let content_index = no_content();
        let now = Instant::now();
        let mut queue = PendingUpserts::default();

        apply_settled(
            file.clone(),
            PendingUpsert {
                deadline: now,
                first_seen: now,
                fingerprint: None,
            },
            now,
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
        );
        assert_eq!(queue.len(), 1, "fresh file must be observed again");
        assert_eq!(metadata.read().len(), 0);

        let queued = queue.due(now + FRESH_RETRY).pop().unwrap().1;
        apply_settled(
            file,
            queued,
            now + FRESH_RETRY,
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
        );
        assert_eq!(queue.len(), 0, "stable file should stop retrying");
        assert_eq!(metadata.read().len(), 1);
    }

    #[test]
    fn rename_event_moves_metadata_and_delete_event_removes_it() {
        let dir = temp_dir("rename");
        let old = dir.join("old.txt");
        let new = dir.join("new.txt");
        std::fs::write(&old, "hello").unwrap();
        // See `create_event_waits_for_settle_before_indexing`.
        let cfg = KensakuConfig {
            watched_dirs: vec![dir.clone()],
            ..KensakuConfig::default()
        };
        let metadata = store(&dir);
        let content_index = no_content();

        handle_file_upsert(&old, &metadata, &content_index);
        assert!(metadata.read().get(&old).is_some());

        std::fs::rename(&old, &new).unwrap();
        let mut queue = PendingUpserts::default();
        handle_event(
            &event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[&old, &new],
            ),
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
            Instant::now(),
        );
        assert!(metadata.read().get(&old).is_none());
        settle_old(new.clone(), &cfg, &metadata, &content_index);
        assert!(metadata.read().get(&new).is_some());

        std::fs::remove_file(&new).unwrap();
        handle_event(
            &event(EventKind::Remove(RemoveKind::File), &[&new]),
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
            Instant::now(),
        );
        assert!(metadata.read().get(&new).is_none());
    }

    #[test]
    fn pause_still_applies_deletes_and_renames() {
        let dir = temp_dir("pause-delete");
        let file = dir.join("gone.txt");
        std::fs::write(&file, "hello").unwrap();
        let cfg = KensakuConfig {
            pause_indexing: true,
            // See `create_event_waits_for_settle_before_indexing`.
            watched_dirs: vec![dir.clone()],
            ..KensakuConfig::default()
        };
        let metadata = store(&dir);
        let content_index = no_content();
        handle_file_upsert(&file, &metadata, &content_index);
        std::fs::remove_file(&file).unwrap();

        let mut queue = PendingUpserts::default();
        handle_event(
            &event(EventKind::Remove(RemoveKind::File), &[&file]),
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
            Instant::now(),
        );
        assert!(metadata.read().get(&file).is_none());
    }

    #[test]
    fn writes_inside_kensakus_runtime_dir_are_never_indexed() {
        // On macOS the config dir (index, SQLite WAL, thumbnails, OCR packs)
        // is not a hidden path; the watcher must ignore every event from it
        // just like the crawler does (`runtime_dir_prunes`).
        let runtime_dir = crate::indexer::walker::runtime_dir_prunes()
            .pop()
            .expect("there is always at least one prune root");
        let marker = runtime_dir.join("index-wal.txt");

        // The event target lives in the real runtime dir, but the test's
        // backing stores are scratch directories — the prune decision is made
        // purely from the path before any store is touched.
        let tmp = temp_dir("runtime-prune");
        let cfg = KensakuConfig::default();
        let metadata = store(&tmp);
        let content_index = no_content();
        let mut queue = PendingUpserts::default();
        let now = Instant::now();

        let changes = handle_event(
            &event(EventKind::Create(CreateKind::File), &[&marker]),
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
            now,
        );
        assert_eq!(changes, 0, "created, but inside Kensaku's own storage");
        assert_eq!(queue.len(), 0, "must not be queued for settle");

        handle_event(
            &event(EventKind::Remove(RemoveKind::Any), &[&marker]),
            &cfg,
            &metadata,
            &content_index,
            &mut queue,
            now,
        );
        assert_eq!(queue.len(), 0);
        assert_eq!(metadata.read().len(), 0);
    }

    #[test]
    fn hidden_entries_survive_rescan_until_skip_hidden_is_enabled() {
        let root = temp_dir("hidden-rescan");
        let data = temp_dir("hidden-rescan-store");
        let hidden = root.join(".notes/note.txt");
        let visible = root.join("visible.txt");
        std::fs::create_dir_all(hidden.parent().unwrap()).unwrap();
        std::fs::write(&hidden, "hidden note").unwrap();
        std::fs::write(&visible, "visible note").unwrap();
        let mut cfg = KensakuConfig {
            watched_dirs: vec![root.clone()],
            skip_hidden: false,
            include_ignored: true,
            ..KensakuConfig::default()
        };
        let metadata = store(&data);
        let content_index = no_content();
        rescan_watched_dirs(std::slice::from_ref(&root), &cfg, &metadata, &content_index);
        assert!(metadata.read().get(&hidden).is_some());
        assert!(metadata.read().get(&visible).is_some());
        cfg.skip_hidden = true;
        rescan_watched_dirs(std::slice::from_ref(&root), &cfg, &metadata, &content_index);
        assert!(metadata.read().get(&hidden).is_none());
        assert!(metadata.read().get(&visible).is_some());
        drop(metadata);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn hidden_descendants_are_discovered_during_directory_rename_repair() {
        let root = temp_dir("hidden-rename");
        let data = temp_dir("hidden-rename-store");
        let old = root.join("old");
        let new = root.join(".renamed");
        std::fs::create_dir_all(old.join(".nested")).unwrap();
        std::fs::write(old.join(".nested/payload.txt"), "fixture").unwrap();
        let cfg = KensakuConfig {
            watched_dirs: vec![root.clone()],
            skip_hidden: false,
            include_ignored: true,
            ..KensakuConfig::default()
        };
        let metadata = store(&data);
        let content_index = no_content();
        // Only the old directory is known. Its hidden descendant must be
        // discovered by the asynchronous repair walk, not a prefix rewrite.
        handle_file_upsert(&old, &metadata, &content_index);
        std::fs::rename(&old, &new).unwrap();
        handle_directory_rename(&old, &new, &cfg, &metadata, &content_index);
        let expected = new.join(".nested/payload.txt");
        let deadline = Instant::now() + Duration::from_secs(5);
        while metadata.read().get(&expected).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            metadata.read().get(&expected).is_some(),
            "repair lost hidden descendant"
        );
        assert!(metadata.read().get(&old).is_none());
        // The worker owns cloned stores until it finishes; leave scratch
        // cleanup to the OS rather than race its final transaction flush.
    }

    #[test]
    fn overflow_rescan_adds_missing_and_drops_ghosts() {
        let dir = temp_dir("rescan");
        let live = dir.join("live.txt");
        let ghost = dir.join("ghost.txt");
        std::fs::write(&live, "live").unwrap();
        std::fs::write(&ghost, "ghost").unwrap();
        let cfg = KensakuConfig {
            watched_dirs: vec![dir.clone()],
            ..KensakuConfig::default()
        };
        let metadata = store(&dir);
        let content_index = no_content();
        handle_file_upsert(&ghost, &metadata, &content_index);
        std::fs::remove_file(&ghost).unwrap();

        let changed =
            rescan_watched_dirs(std::slice::from_ref(&dir), &cfg, &metadata, &content_index);
        assert!(changed >= 2);
        assert!(metadata.read().get(&live).is_some());
        assert!(
            metadata.read().get(&ghost).is_none(),
            "overflow rescan must remove ghosts"
        );
    }

    #[test]
    fn rescans_are_throttled() {
        let now = Instant::now();
        assert!(rescan_due(None, now), "the first overflow always rescans");
        assert!(rescan_due(Some(now - RESCAN_COOLDOWN), now));
        assert!(
            !rescan_due(Some(now - Duration::from_secs(1)), now),
            "a second overflow inside the cooldown must be ignored"
        );
    }

    #[test]
    fn unchanged_rescan_does_not_reindex_the_tree() {
        let root = temp_dir("rescan-unchanged");
        // Keep the SQLite database outside the watched root: writes to it
        // would otherwise change the root's own mtime and make this flaky.
        let data = temp_dir("rescan-unchanged-store");
        std::fs::write(root.join("stable.txt"), "stable").unwrap();
        let cfg = KensakuConfig {
            watched_dirs: vec![root.clone()],
            ..KensakuConfig::default()
        };
        let metadata = store(&data);
        let content_index = no_content();

        let first =
            rescan_watched_dirs(std::slice::from_ref(&root), &cfg, &metadata, &content_index);
        assert!(first > 0, "the first rescan must pick up the new tree");
        let entries = metadata.read().len();

        let second =
            rescan_watched_dirs(std::slice::from_ref(&root), &cfg, &metadata, &content_index);
        assert_eq!(second, 0, "an unchanged tree must not be re-indexed");
        assert_eq!(metadata.read().len(), entries);
    }
}
