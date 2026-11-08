pub mod agent;
pub mod cli;
pub mod commands;
pub mod config;
pub mod crashlog;
pub mod daemon;
pub mod doctor;
pub mod embed;
pub mod indexer;
pub mod models;
pub mod main_window;
pub mod maintenance;
pub mod ocr;
pub mod ocr_indra;
pub mod preview;
pub mod query;
pub mod search;
pub mod tray_icon;
pub mod updater;
pub mod util;

use config::KensakuConfig;
use indexer::IndexerManager;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use tauri::{
    Manager,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

pub struct AppState {
    pub indexer: Arc<IndexerManager>,
    pub daemon: Option<crate::daemon::DaemonClient>,
}

/// Owns the single-instance lock for the whole process lifetime: the OS releases
/// the advisory lock when the process exits, and dropping the `File` earlier
/// would let a second instance start.
static INSTANCE_LOCK: OnceLock<std::sync::Mutex<Option<std::fs::File>>> = OnceLock::new();

/// Releases the single-instance lock so the next process can take it.
///
/// `AppHandle::restart()` spawns the replacement process before this one
/// exits. A process that still holds the lock makes the fresh instance lose
/// the race and quit, which is how a reset could close the app and never
/// bring it back. Called only on an explicit, user-confirmed reset.
pub fn release_instance_lock() {
    if let Some(slot) = INSTANCE_LOCK.get()
        && let Ok(mut guard) = slot.lock()
    {
        // Dropping the handle releases the OS lock; removing the file keeps a
        // stale inode from confusing the next startup.
        *guard = None;
    }
    let _ = std::fs::remove_file(instance_lock_path());
}

/// The GUI's own lock, separate from `kensaku.lock`.
///
/// `kensaku.lock` belongs to whichever process owns the index (the daemon when
/// it runs). The GUI used to skip locking entirely in that case, so two
/// windows, two trays and two shortcut registrations could coexist and
/// `--toggle` booted a second app instead of summoning the first.
fn instance_lock_path() -> PathBuf {
    KensakuConfig::config_dir().join("kensaku-gui.lock")
}

/// Tries to take the GUI exclusive lock on `<config_dir>/kensaku-gui.lock`.
///
/// * `Ok(Some(file))` — acquired; the caller must keep `file` alive.
/// * `Ok(None)` — another instance already holds the lock.
/// * `Err(_)` — the lock file could not be created or opened. Startup continues
///   without the guard rather than bricking the app on a read-only config dir.
fn acquire_instance_lock() -> Result<Option<std::fs::File>, String> {
    let dir = KensakuConfig::config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))?;

    let path = instance_lock_path();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("{}: {}", path.display(), e))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {}", path.display(), e)),
    }
}

/// Request file a second launch writes to ask the running app to show (or
/// toggle) its window. A file works on every OS and needs no sockets; the
/// running app polls for it.
fn window_request_path() -> PathBuf {
    KensakuConfig::config_dir().join("window.request")
}

/// Asks the running app for its window, then exits.
///
/// A normal launch (Start menu, desktop shortcut) means "show the window".
/// `--toggle` means show-or-hide, used by desktop shortcuts on Linux where
/// global keys may be blocked.
fn request_window_and_exit(command: &str) -> ! {
    let path = window_request_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, command);
    std::process::exit(0);
}

/// Acquires the lock, exiting quietly when another instance is running.
fn acquire_instance_lock_or_exit() -> Option<std::fs::File> {
    match acquire_instance_lock() {
        Ok(Some(file)) => Some(file),
        Ok(None) => {
            // Closing to the tray keeps the process alive, so a launch from the
            // Start menu would otherwise hit this guard and exit without
            // showing anything. Ask the running instance for its window.
            let toggle = std::env::args().any(|a| a == "--toggle");
            request_window_and_exit(if toggle { "toggle" } else { "show" });
        }
        Err(e) => {
            eprintln!(
                "[Kensaku] Could not acquire single-instance lock ({}); continuing without it",
                e
            );
            None
        }
    }
}

/// Marker consumed on the next launch to wipe all Kensaku state.
fn reset_marker_path_in(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join(".reset-requested")
}

fn reset_marker_path() -> std::path::PathBuf {
    reset_marker_path_in(&KensakuConfig::config_dir())
}

/// True when a reset was requested but not applied yet.
pub fn reset_pending() -> bool {
    reset_marker_path().exists()
}

/// How many times a pending reset is retried, and how long between attempts.
///
/// A reset is only retried when deleting something failed. On a normal start
/// there is no marker and none of this runs.
const RESET_ATTEMPTS: u32 = 20;
const RESET_RETRY: std::time::Duration = std::time::Duration::from_millis(150);

/// Runs `reset` until it succeeds or the attempt budget runs out.
///
/// Split out so the retry contract is testable without having to hold real
/// files open. Returns the last error when every attempt failed.
fn reset_with_retries<F>(
    attempts: u32,
    delay: std::time::Duration,
    mut reset: F,
) -> Result<(), String>
where
    F: FnMut() -> Result<(), String>,
{
    let mut last = String::new();
    for attempt in 1..=attempts {
        match reset() {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = error;
                if attempt < attempts {
                    std::thread::sleep(delay);
                }
            }
        }
    }
    Err(last)
}

/// Applies a reset marker in `dir`. Returns true when a marker was found and
/// the directory was cleared/recreated. Split from the startup path so the
/// marker/restart contract has an automated test.
fn apply_pending_reset_in(dir: &std::path::Path) -> bool {
    let marker = reset_marker_path_in(dir);
    if !marker.exists() {
        return false;
    }

    // Downloaded OCR and model packs survive a reset unless the user asked
    // for them explicitly (see `ResetScope`). Wiping the whole directory used
    // to throw away ~55 MB of downloads that are not the user's data, so
    // "reset to fix a bad index" cost a re-download before search worked.
    let scope = maintenance::ResetScope {
        remove_packs: reset_marker_removes_packs(dir),
    };

    // The replacement process is spawned before the old one exits, so the
    // previous process can still hold the index's memory maps when this runs.
    // Windows refuses to delete a mapped file, which left the marker in place
    // and made the app reset itself again on the next launch. Retry briefly:
    // the old process is already exiting and releases everything in moments.
    let result = reset_with_retries(RESET_ATTEMPTS, RESET_RETRY, || {
        maintenance::apply_reset(dir, scope)
    });

    match result {
        Ok(()) => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!(
                    "[Kensaku] Reset requested but could not recreate {}: {}",
                    dir.display(),
                    e
                );
                return true;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
            let _ = std::fs::remove_file(&marker);
            eprintln!(
                "[Kensaku] Reset requested: cleared user data in {}{}",
                dir.display(),
                if scope.remove_packs {
                    " (including downloaded packs)"
                } else {
                    " (downloaded packs kept)"
                }
            );
        }
        Err(e) => eprintln!(
            "[Kensaku] Reset requested but could not clear everything in {}: {}",
            dir.display(),
            e
        ),
    }
    true
}

/// True when the reset marker asks for the downloaded packs to go as well.
///
/// The marker's contents carry the choice, so the decision is made by the UI
/// that asked for the reset and applied by the fresh process before anything
/// is open.
fn reset_marker_removes_packs(dir: &std::path::Path) -> bool {
    std::fs::read_to_string(reset_marker_path_in(dir))
        .map(|contents| contents.trim().eq_ignore_ascii_case("full"))
        .unwrap_or(false)
}

/// Clears Kensaku state when a reset was requested by `reset_all`.
///
/// Must run before `KensakuConfig::load()` and before `IndexerManager::new()`:
/// removing the directory while the database or index is open would leave
/// handles pointing at deleted inodes.
fn rebuild_marker_path_in(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join(".rebuild-requested")
}

fn rebuild_marker_path() -> std::path::PathBuf {
    rebuild_marker_path_in(&KensakuConfig::config_dir())
}

pub fn rebuild_pending() -> bool {
    rebuild_marker_path().exists()
}

fn apply_pending_rebuild_in(dir: &std::path::Path) -> bool {
    let marker = rebuild_marker_path_in(dir);
    if !marker.exists() {
        return false;
    }
    // Keep settings (config.json); wipe only the index and metadata.
    let _ = std::fs::remove_dir_all(dir.join("index"));
    let _ = std::fs::remove_dir_all(dir.join("thumbs"));
    for name in ["metadata.db", "metadata.db-wal", "metadata.db-shm"] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    let _ = std::fs::remove_file(&marker);
    eprintln!("[Kensaku] Rebuild requested: index cleared in {}", dir.display());
    true
}

fn maybe_apply_pending_rebuild() {
    apply_pending_rebuild_in(&KensakuConfig::config_dir());
}

fn maybe_apply_pending_reset() {
    apply_pending_reset_in(&KensakuConfig::config_dir());
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Install the crash log hook before anything else so startup failures are
    // captured too. It only touches the config directory path; it never loads
    // or mutates the config.
    crashlog::install_panic_hook();
    crashlog::info(&format!(
        "starting {} {} ({} {})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

    // `--remove-data` deletes everything Kensaku stores and exits, without
    // opening the window or the stores. This is what an uninstall does on
    // platforms where the installer has no uninstall hook (Linux, macOS), and
    // what a user can run by hand. Windows still gets the NSIS hook.
    if std::env::args().any(|arg| arg == "--remove-data") {
        match maintenance::remove_all_data() {
            Ok(report) => {
                crate::say!(
                    "Removed Kensaku data from {} ({} reclaimed).",
                    report.path,
                    report.human_bytes()
                );
            }
            Err(error) => {
                crate::say_err!("Could not remove Kensaku data: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    commands::init_process_start();

    maybe_apply_pending_rebuild();
    maybe_apply_pending_reset();

    // Reclaim what past upgrades left behind. Best-effort: a failure here must
    // never stop startup.
    let cleaned = maintenance::cleanup_stale_artifacts();
    if cleaned.bytes_reclaimed > 0 || !cleaned.errors.is_empty() {
        crate::say!(
            "[Kensaku] cleanup: removed {} legacy index backup(s), reclaimed {} bytes{}",
            cleaned.legacy_backups_removed,
            cleaned.bytes_reclaimed,
            if cleaned.update_temp_cleared {
                ", cleared staged updates"
            } else {
                ""
            }
        );
        for error in &cleaned.errors {
            crate::say_err!("[Kensaku] cleanup: {error}");
        }
    }

    #[cfg(unix)]
    let daemon_available = crate::daemon::DaemonClient::ensure();
    #[cfg(not(unix))]
    let daemon_available = false;

    // The GUI always takes its own lock, daemon or not. The daemon lock is a
    // different file: it guards the index owner, not the window.
    let instance_lock = acquire_instance_lock_or_exit();

    if let Some(file) = instance_lock {
        let _ = INSTANCE_LOCK.set(std::sync::Mutex::new(Some(file)));
    }

    let config = KensakuConfig::load();
    crashlog::info(&format!(
        "config loaded: setup_complete={} app_mode={} ocr_enabled={} engine={}",
        config.is_setup_complete, config.app_mode, config.ocr_enabled, config.ocr_engine
    ));
    indexer::extract::set_ocr_enabled(config.ocr_enabled);
    indexer::extract::image::set_ocr_max_image_bytes(config.ocr_max_image_bytes);
    indexer::extract::set_ocr_languages(config.ocr_languages.clone());
    indexer::extract::set_content_caps(config.content_max_file_size_mb, config.content_max_text_bytes);
    crate::embed::set_max_semantic_units(config.content_max_semantic_units);
    ocr::set_engine(&config.ocr_engine);

    // Self-heal on launch, without surprise downloads. HTTP downloads only
    // happen when the user actually enabled OCR:
    // - Tesseract language data only when the Tesseract binary is installed
    //   but has no language. Without the binary the data is useless.
    // - The PP-OCR (or Indra) pack only when OCR is on. (A crawl also fetches
    //   it when needed; see `IndexerManager::sync_ocr_state`.)
    if config.ocr_enabled {
        std::thread::spawn(|| {
            use crate::indexer::extract::image;
            if image::tesseract_available() && !image::tesseract_usable() {
                let _ = image::download_eng_tessdata();
            }
        });
        if crate::ocr::accurate_compiled() && crate::ocr::runtime_loadable() {
            // One download at a time: the selected engine's pack first.
            if crate::ocr::engine_name() == crate::ocr::INDRA && !crate::ocr_indra::pack_present() {
                let _ = crate::models::download_indra_pack();
            } else if !crate::ocr::pack_present() {
                let _ = crate::models::download_ocr_pack();
            }
        }
    }

    // OCR engine selection (switching to PP-OCR when Tesseract is missing,
    // fetching its pack) happens at the start of every crawl, in
    // `IndexerManager::sync_ocr_state`, so it also runs in the daemon and
    // re-queues already-indexed images.

    let is_setup = config.is_setup_complete;
    // Desktop-app mode launches with its window visible; launcher mode starts
    // in the tray. Pending first-run setup always needs UI.
    let open_window_on_start = !is_setup || config.app_mode == "app";

    let (indexer_manager, daemon_client) = if daemon_available {
        (
            Arc::new(IndexerManager::new_readonly(config.clone())),
            Some(crate::daemon::DaemonClient),
        )
    } else {
        let manager = Arc::new(IndexerManager::new(config.clone()));
        if is_setup {
            manager.start_indexing();
        }
        (manager, None)
    };

    // When the PP-OCR pack finishes downloading, OCR every image that was
    // indexed without it. In daemon mode the daemon owns the index: re-sending
    // the config makes it notice the new OCR signature and crawl.
    {
        let manager = Arc::clone(&indexer_manager);
        let daemon = daemon_client.is_some();
        models::on_ocr_pack_ready(move || {
            if daemon {
                let config = manager.config.read().clone();
                if let Err(e) = crate::daemon::DaemonClient.save_config(config) {
                    eprintln!("[Kensaku OCR] could not notify the daemon: {e}");
                }
            } else {
                manager.start_indexing_when_idle();
            }
        });
    }

    // When the semantic model files finish downloading, make the new model
    // active straight away and rebuild vectors that belonged to the previous
    // one. Without this the files sat on disk while search kept using the
    // hashing fallback until the next launch.
    {
        let manager = Arc::clone(&indexer_manager);
        models::on_semantic_models_ready(move || {
            manager.on_semantic_model_changed();
        });
    }

    let app_state = AppState {
        indexer: indexer_manager,
        daemon: daemon_client,
    };

    let shortcut_str = config.shortcut.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        main_window::toggle(app);
                    }
                })
                .build(),
        )
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::legacy_search,
            commands::get_indexing_status,
            commands::get_stats,
            commands::set_theme,
            commands::open_file,
            commands::open_folder,
            commands::get_file_preview,
            commands::get_snippets,
            commands::get_thumbnail,
            commands::get_thumbnail_data,
            commands::get_file_details,
            commands::get_ocr_status,
            commands::download_ocr_language,
            commands::get_ocr_pack_status,
            commands::download_ocr_pack,
            commands::get_indra_pack_status,
            commands::download_indra_pack,
            commands::get_smart_pack_status,
            commands::download_smart_pack,
            commands::get_visual_pack_status,
            commands::download_visual_pack,
            commands::find_duplicates,
            commands::find_similar_images,
            commands::execute_command,
            commands::read_file_content,
            commands::get_config,
            commands::save_config,
            commands::is_setup_complete,
            commands::apply_window_mode,
            commands::is_setup_configured,
            commands::run_setup_again,
            commands::complete_setup,
            commands::get_suggested_directories,
            commands::validate_directory,
            commands::get_scope_suggestions,
            commands::get_home_data,
            commands::toggle_pin,
            commands::hide_window,
            commands::remember_query,
            commands::record_recent_query,
            commands::last_query,
            commands::get_recent_queries,
            commands::clear_recent_queries,
            commands::run_doctor,
            commands::reset_all,
            commands::rebuild_index,
            commands::run_doctor_action,
            commands::get_reset_pending,
            commands::get_autostart,
            commands::set_autostart,
            commands::agent_connect_status,
            commands::agent_connect_install,
            commands::agent_connect_snippet,
            commands::check_for_updates,
            commands::download_update,
            commands::get_updater_status,
            commands::install_update_and_restart,
            models::get_models_status,
            models::download_models,
            models::delete_models,
        ])
        .setup(move |app| {
            // Watch for a second launch (`--toggle`, or a plain Start-menu
            // click that must reopen the hidden window). It drops a request
            // file; a plain launch shows, `--toggle` toggles. Cheap: one small
            // read every 150 ms, and a normal launch has no file to read.
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    let path = window_request_path();
                    let Ok(request) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    let _ = std::fs::remove_file(&path);
                    if request.trim() == "toggle" {
                        main_window::toggle(&handle);
                    } else {
                        main_window::show(&handle);
                    }
                });
            }

            if app.state::<AppState>().daemon.is_none() {
                app.state::<AppState>()
                    .indexer
                    .set_app_handle(app.handle().clone());
            }

            // 1. Register global shortcut (e.g. Alt+Space). The summon shortcut
            // only makes sense in launcher mode; full-app mode stays open.
            if config.app_mode == "launcher" {
                if let Ok(shortcut) = Shortcut::from_str(&shortcut_str) {
                    if let Err(e) = app.global_shortcut().register(shortcut) {
                        eprintln!("[Kensaku] Could not register global shortcut '{}': {}", shortcut_str, e);
                    }
                } else {
                    eprintln!(
                        "[Kensaku] Could not parse global shortcut: {}",
                        shortcut_str
                    );
                }
            }

            // 2. Setup System Tray
            let show_item = MenuItem::with_id(app, "toggle", "Open Search", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit Kensaku", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            // Windows users expect a click on the tray icon to offer the
            // menu (Open / Quit), and a double-click to open. Elsewhere a
            // left click toggles the window and the menu is a right click.
            let mut tray_builder = TrayIconBuilder::with_id("main")
                .menu(&tray_menu)
                .show_menu_on_left_click(cfg!(windows))
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "toggle" => main_window::toggle(app),
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| match event {
                    // Click fires on press and on release. Acting on both
                    // toggled twice, so the window opened and closed again.
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } if !cfg!(windows) => main_window::toggle(tray.app_handle()),
                    TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } => main_window::show(tray.app_handle()),
                    _ => {}
                });

            // On Windows, pick the initial tray icon that matches the app theme so
            // the icon is visible on both light and dark taskbars.  Other platforms
            // keep the default window icon (Tauri picks the best size automatically).
            #[cfg(windows)]
            {
                let icon_bytes = tray_icon::tray_icon_bytes(&config.theme);
                if let Some(icon) =
                    tray_icon::decode_png(icon_bytes).or_else(|| app.default_window_icon().cloned())
                {
                    tray_builder = tray_builder.icon(icon);
                }
            }
            #[cfg(not(windows))]
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }

            let _ = tray_builder.build(app);

            // 3. Launcher mode starts in the background (tray, watcher and
            // indexer only); the window is created by the first toggle
            // (Alt+Space, tray click). Desktop-app mode is an ordinary app:
            // show its window on launch. A pending first-run setup always
            // needs UI right away.
            if open_window_on_start {
                // Apply the window mode before the first paint so a launcher
                // window never flashes decorated (or, before setup is done,
                // never appears as a palette that cannot be minimised).
                if let Some(window) = app.get_webview_window(main_window::LABEL) {
                    main_window::apply_mode(&window, &config.app_mode);
                }
                main_window::show(app.handle());
            }

            // A dev session may have registered the debug binary for
            // sign-in; that would open a console at every login.
            commands::self_heal_autostart();

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running Kensaku application")
        .run(|_app, event| {
            // Destroying the search window (hide) must not end the app. The
            // indexer, watcher and tray keep running. Only the tray's Quit
            // (app.exit) ends it, and that call is not prevented here.
            if let tauri::RunEvent::ExitRequested { api, code: None, .. } = event {
                api.prevent_exit();
            }
        });
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pending_reset_clears_and_recreates_the_directory() {
        let dir = std::env::temp_dir().join(format!(
            "kensaku-reset-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("metadata.db"), b"stale").unwrap();
        std::fs::write(reset_marker_path_in(&dir), b"1").unwrap();
        assert!(reset_pending_at(&dir));

        assert!(apply_pending_reset_in(&dir));
        assert!(dir.is_dir());
        assert!(!reset_marker_path_in(&dir).exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        // A second call without a marker is a no-op.
        assert!(!apply_pending_reset_in(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_retries_while_the_old_process_still_holds_the_index() {
        let mut calls = 0;
        let result = reset_with_retries(5, std::time::Duration::ZERO, || {
            calls += 1;
            if calls < 3 {
                Err("index is mapped by another process".to_string())
            } else {
                Ok(())
            }
        });

        assert!(result.is_ok());
        assert_eq!(calls, 3, "it must stop retrying once the reset works");
    }

    #[test]
    fn reset_reports_the_last_error_when_every_attempt_fails() {
        let mut calls = 0;
        let result = reset_with_retries(3, std::time::Duration::ZERO, || {
            calls += 1;
            Err(format!("busy {calls}"))
        });

        assert_eq!(result, Err("busy 3".to_string()));
        assert_eq!(calls, 3);
    }

    #[test]
    fn reset_keeps_downloaded_packs_unless_asked_for_a_full_reset() {
        let dir = std::env::temp_dir().join(format!(
            "kensaku-reset-packs-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(dir.join("models/embed.onnx"), b"pack").unwrap();
        std::fs::create_dir_all(dir.join("index")).unwrap();
        std::fs::write(dir.join("metadata.db"), b"stale").unwrap();

        // Default reset: index and database go, downloaded packs stay.
        std::fs::write(reset_marker_path_in(&dir), b"keep-packs").unwrap();
        assert!(apply_pending_reset_in(&dir));
        assert!(dir.join("models/embed.onnx").exists());
        assert!(!dir.join("metadata.db").exists());
        assert!(!dir.join("index").exists());

        // Explicit full reset: packs go too.
        std::fs::write(reset_marker_path_in(&dir), b"full").unwrap();
        assert!(apply_pending_reset_in(&dir));
        assert!(!dir.join("models").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn reset_pending_at(dir: &std::path::Path) -> bool {
        reset_marker_path_in(dir).exists()
    }
}
