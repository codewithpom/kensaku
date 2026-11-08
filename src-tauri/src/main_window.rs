//! The one place the main search window is created, shown, and hidden.
//!
//! Hiding destroys the webview, so the WebKit processes exit and the hidden
//! app keeps only the Rust side (indexer, watcher, daemon client). Showing
//! creates the window again from the same settings. The last query is kept in
//! Rust so the search comes back where the user left it.

use std::collections::VecDeque;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

pub const LABEL: &str = "main";

static LAST_QUERY: Mutex<String> = Mutex::new(String::new());
static RECENT_QUERIES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

pub fn remember_query(query: &str) {
    if let Ok(mut q) = LAST_QUERY.lock() {
        q.clear();
        q.push_str(query);
    }
}

pub fn record_recent_query(query: &str) {
    let trimmed = query.trim();
    if trimmed.starts_with('/') {
        return;
    }
    let count = trimmed.chars().count();
    if count == 0 {
        return;
    }
    if count == 1 {
        let ch = trimmed.chars().next().unwrap_or_default();
        if !crate::indexer::tokenizer::is_cjk(ch) {
            return;
        }
    }
    let mut list = match RECENT_QUERIES.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let trimmed_lower = trimmed.to_lowercase();
    // Case-insensitive deduplication and supersede exact prefix extensions
    list.retain(|q| {
        let q_lower = q.to_lowercase();
        q_lower != trimmed_lower && !trimmed_lower.starts_with(&format!("{q_lower} "))
    });
    list.push_front(trimmed.to_string());
    if list.len() > 5 {
        list.pop_back();
    }
}

pub fn recent_queries() -> Vec<String> {
    let list = match RECENT_QUERIES.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    list.iter().cloned().collect()
}

pub fn clear_recent_queries() {
    let mut list = match RECENT_QUERIES.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    list.clear();
}

pub fn last_query() -> String {
    LAST_QUERY.lock().map(|q| q.clone()).unwrap_or_default()
}

fn existing(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(LABEL)
}

fn create(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    // In "app" mode the window is a normal application window: decorated,
    // opaque, in the dock/taskbar, and not pinned above other windows. In
    // "launcher" mode it stays frameless, off the taskbar, and floats like a
    // palette. (`app_mode` itself defaults to launcher in the config so
    // existing installs keep their behavior; Setup defaults new users to app.)
    //
    // Before setup is complete the window is always created as an ordinary
    // app window. The first-run wizard can index for a long time, so it has to
    // be minimisable and findable in the dock/taskbar; a frameless, off-taskbar,
    // always-on-top palette cannot be minimised or restored, and that is what a
    // fresh install used to get while the wizard was running. The user's mode
    // choice is applied when the wizard closes (`apply_window_mode`).
    let config = crate::config::KensakuConfig::load();
    let in_setup = !config.is_setup_complete;
    let launcher = !in_setup && config.app_mode != "app";
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("Kensaku")
        .inner_size(920.0, 640.0)
        .min_inner_size(720.0, 520.0)
        .decorations(!launcher)
        .transparent(launcher)
        .always_on_top(launcher)
        .skip_taskbar(launcher)
        .visible_on_all_workspaces(launcher)
        .center()
        .shadow(true)
        .resizable(true)
        .visible(false)
        .build()?;

    // Closing the window hides it and keeps the app running in the tray.
    //
    // App mode used to call `exit(0)` here, which threw away the loaded
    // metadata database, full-text index and semantic units, and paid for them
    // again on the next open. On a large corpus that is several seconds per
    // open, for a window the user is about to close again. Hiding keeps the
    // Rust side warm; `hide` destroys the webview and trims memory, so what
    // stays resident is the index, not a UI.
    //
    // Quitting stays explicit: the tray's Quit item, or `close_to_background`
    // turned off. Both modes now behave the same way, because both are
    // background-resident apps; the mode only decides what the window looks
    // like.
    //
    // Config is read inside the handler so a Settings change applies to the
    // live window without recreating it.
    let app_handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            if !crate::config::KensakuConfig::load().close_to_background {
                app_handle.exit(0);
                return;
            }
            api.prevent_close();
            hide(&app_handle);
        }
    });
    Ok(window)
}

/// Applies the parts of `app_mode` that can change on a live window. App mode
/// gets native decorations (title bar, minimize/close) so it behaves like a
/// normal desktop window; launcher mode stays frameless.
pub fn apply_mode(window: &WebviewWindow, app_mode: &str) {
    // While the setup wizard is up, the window stays an ordinary app window
    // whatever mode is configured: a frameless, off-taskbar, always-on-top
    // palette cannot be minimised or restored, and the wizard sits there for
    // the whole first scan. `apply_window_mode` runs again when it closes.
    // `is_setup_complete` rather than `is_setup_done`: the latter also waits
    // for the first scan to finish, and the wizard closes before that, so the
    // user's chosen mode would be ignored until the next launch.
    let setup_complete = crate::config::KensakuConfig::load().is_setup_complete;
    let launcher = app_mode != "app" && setup_complete;
    let _ = window.set_decorations(!launcher);
    let _ = window.set_skip_taskbar(launcher);
    let _ = window.set_always_on_top(launcher);
    let _ = window.set_visible_on_all_workspaces(launcher);
}

/// Tells the user why the app has no window, even when release builds run
/// without a console. The log path is included so a bug report is actionable.
fn report_creation_failure(app: &AppHandle, error: &str) {
    crate::crashlog::record_error("main window creation", error);
    crate::say_err!("[Kensaku] could not create the window: {error}");

    use tauri_plugin_dialog::DialogExt;
    let hint = if cfg!(windows) {
        "\n\nThis usually means the Microsoft Edge WebView2 runtime is missing or broken. Install it from Microsoft, then try again."
    } else {
        ""
    };
    // `blocking_show` must not run on the main thread, and this is called from
    // `.setup` and tray handlers. `show` is non-blocking and posts the dialog
    // to the platform's event loop.
    app.dialog()
        .message(format!(
            "Kensaku could not open its window.\n\n{error}{hint}\n\nDetails were written to:\n{}",
            crate::crashlog::path().display()
        ))
        .title("Kensaku could not start")
        .show(|_| {});
}

/// Shows the search window, creating it if it was hidden (destroyed).
pub fn show(app: &AppHandle) {
    let window = match existing(app) {
        Some(w) => w,
        None => match create(app) {
            Ok(w) => w,
            Err(e) => {
                report_creation_failure(app, &e.to_string());
                return;
            }
        },
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// Hides the search window by destroying it, then returns freed memory.
pub fn hide(app: &AppHandle) {
    if let Some(window) = existing(app) {
        // Hide first, then destroy: on Windows, tearing down a visible
        // transparent + always-on-top window while WebView2 crashes out (the
        // process list is killed below) can briefly leave a composited ghost
        // frame on screen. Once hidden, nothing of the window is visible.
        let _ = window.hide();
        let _ = window.destroy();
    }
    // WebKit/WebView helpers otherwise stay alive after the window is
    // destroyed, keeping tens to hundreds of MB resident. Kill them and trim
    // the allocator so the hidden app keeps only the Rust side.
    crate::util::memory::kill_webview_processes();
    crate::util::memory::release_memory();
}

/// Shows the window if it is hidden, hides it if it is shown.
///
/// Desktop-app windows are never hidden by a toggle: the tray item, a stray
/// `--toggle`, or a shortcut registered before a mode change should bring the
/// window forward, not make a normal app vanish from the taskbar.
pub fn toggle(app: &AppHandle) {
    let visible = existing(app).is_some_and(|w| w.is_visible().unwrap_or(false));
    if visible && crate::config::KensakuConfig::load().app_mode != "app" {
        hide(app);
    } else {
        show(app);
    }
}

/// Returns freed heap pages to the OS. glibc otherwise keeps them, so the
/// process can report more memory than it still uses.
pub fn release_free_memory() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_queries_records_deduplicates_and_caps_at_five() {
        clear_recent_queries();
        assert!(recent_queries().is_empty());

        // Single Latin character and empty strings are ignored
        record_recent_query("");
        record_recent_query("   ");
        record_recent_query("a");
        record_recent_query("  b  ");
        assert!(recent_queries().is_empty());

        // Slash commands are ignored
        record_recent_query("/settings");
        record_recent_query("/doctor");
        assert!(recent_queries().is_empty());

        // Single CJK characters are valid and retained
        record_recent_query("税");
        assert_eq!(recent_queries(), vec!["税"]);
        record_recent_query("书");
        assert_eq!(recent_queries(), vec!["书", "税"]);

        clear_recent_queries();

        // Valid queries
        record_recent_query("alpha");
        record_recent_query("beta");
        record_recent_query("gamma");
        assert_eq!(recent_queries(), vec!["gamma", "beta", "alpha"]);

        // Case-insensitive deduplication moves to front
        record_recent_query("Alpha");
        assert_eq!(recent_queries(), vec!["Alpha", "gamma", "beta"]);

        // Prefix extension superseding
        record_recent_query("Alpha project");
        assert_eq!(recent_queries(), vec!["Alpha project", "gamma", "beta"]);

        // Cap at 5
        record_recent_query("delta");
        record_recent_query("epsilon");
        record_recent_query("zeta");
        assert_eq!(
            recent_queries(),
            vec!["zeta", "epsilon", "delta", "Alpha project", "gamma"]
        );
        assert_eq!(recent_queries().len(), 5);

        clear_recent_queries();
        assert!(recent_queries().is_empty());
    }
}
