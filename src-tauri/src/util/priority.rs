//! Best-effort background priority for crawler and extraction threads.
//!
//! Indexing must never fight typing. `balanced` and `eco` put the crawl thread
//! and extraction workers into the OS idle/background class; `full` restores
//! normal priority. Every
//! syscall is advisory and failures are ignored, so unsupported sandboxes and
//! permission-restricted environments still index correctly.
//!
//! Set `KENSAKU_NO_BG_PRIORITY=1` to disable the policy (useful for tests,
//! profiling, and diagnosing slow crawls).

use crate::config::CrawlMode;
use std::sync::atomic::{AtomicBool, Ordering};

static BACKGROUND: AtomicBool = AtomicBool::new(true);

/// Update policy without changing the calling UI/command thread's priority.
pub fn set_mode(mode: CrawlMode) {
    BACKGROUND.store(mode.is_background(), Ordering::Relaxed);
}

/// Apply the current policy on an extraction worker, once per document.
pub fn apply_current() {
    let mode = if BACKGROUND.load(Ordering::Relaxed) {
        CrawlMode::Balanced
    } else {
        CrawlMode::Full
    };
    apply(mode);
}

#[cfg(test)]
std::thread_local! {
    static BACKGROUND_REQUESTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn apply(crawl_mode: CrawlMode) {
    if std::env::var_os("KENSAKU_NO_BG_PRIORITY").is_some() {
        return;
    }
    let background = crawl_mode.is_background();
    #[cfg(test)]
    if background {
        BACKGROUND_REQUESTS.set(BACKGROUND_REQUESTS.get() + 1);
    }
    apply_platform(background);
}

#[cfg(test)]
pub fn background_requests() -> usize {
    BACKGROUND_REQUESTS.get()
}

#[cfg(target_os = "linux")]
fn apply_platform(background: bool) {
    // SCHED_IDLE is not exported by libc on every Linux target.
    const SCHED_IDLE: libc::c_int = 5;
    const IOPRIO_CLASS_IDLE: libc::c_int = 3;
    const IOPRIO_CLASS_BE: libc::c_int = 2;
    const IOPRIO_BE_NORM: libc::c_int = 4;

    unsafe {
        let param: libc::sched_param = std::mem::zeroed();
        let policy = if background {
            SCHED_IDLE
        } else {
            libc::SCHED_OTHER
        };
        let _ = libc::sched_setscheduler(0, policy, &param);

        let (class, data) = if background {
            (IOPRIO_CLASS_IDLE, 0)
        } else {
            (IOPRIO_CLASS_BE, IOPRIO_BE_NORM)
        };
        let ioprio = (class << 13) | data;
        let _ = libc::syscall(libc::SYS_ioprio_set, 1i32, 0i32, ioprio);
    }
}

#[cfg(target_os = "macos")]
fn apply_platform(background: bool) {
    let priority = if background { libc::PRIO_DARWIN_BG } else { 0 };
    unsafe {
        let _ = libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, priority);
    }
}

#[cfg(windows)]
fn apply_platform(background: bool) {
    // Background mode affects only this worker, including I/O priority.
    // Applying a process priority class also demotes the UI and webview.
    const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
    const THREAD_MODE_BACKGROUND_END: i32 = 0x0002_0000;

    unsafe extern "system" {
        fn GetCurrentThread() -> *mut core::ffi::c_void;
        fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
    }

    let priority = if background {
        THREAD_MODE_BACKGROUND_BEGIN
    } else {
        THREAD_MODE_BACKGROUND_END
    };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), priority);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn apply_platform(_background: bool) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CrawlMode;

    #[test]
    fn only_full_stays_at_normal_priority() {
        assert!(!CrawlMode::Full.is_background());
        assert!(CrawlMode::Balanced.is_background());
        assert!(CrawlMode::Eco.is_background());
    }

    #[test]
    fn full_does_not_request_background_priority() {
        let before = background_requests();
        apply(CrawlMode::Full);
        assert_eq!(background_requests(), before);
    }

    #[cfg(windows)]
    #[test]
    fn worker_background_priority_leaves_process_priority_unchanged() {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, GetCurrentThread, GetPriorityClass, GetThreadPriority,
        };
        std::thread::spawn(|| unsafe {
            let process_priority = GetPriorityClass(GetCurrentProcess());
            let thread_priority = GetThreadPriority(GetCurrentThread());
            apply(CrawlMode::Balanced);
            assert_eq!(GetPriorityClass(GetCurrentProcess()), process_priority);
            apply(CrawlMode::Full);
            assert_eq!(GetPriorityClass(GetCurrentProcess()), process_priority);
            assert_eq!(GetThreadPriority(GetCurrentThread()), thread_priority);
        })
        .join()
        .unwrap();
    }
}
