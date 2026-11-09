use crate::config::KensakuConfig;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub html_url: String,
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub release_name: String,
    pub release_notes: String,
    pub release_url: String,
    pub published_at: String,
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub asset_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProgress {
    pub phase: String,
    pub downloaded: u64,
    pub total: u64,
    pub percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateStatus {
    pub busy: bool,
    pub progress: Option<UpdateProgress>,
    pub error: Option<String>,
    pub downloaded_path: Option<String>,
    pub ready_to_install: bool,
}

/// Compares two version strings (`candidate` vs `current`), stripping any leading
/// 'v' and discarding pre-release / build metadata tags (e.g. "1.0.14-beta").
/// Returns true only if `candidate` is strictly newer.
pub fn is_newer_version(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> Vec<u64> {
        let clean = v
            .trim()
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("");
        clean
            .split('.')
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    }
    let a = parse(candidate);
    let b = parse(current);
    let len = a.len().max(b.len());
    for i in 0..len {
        let val_a = a.get(i).copied().unwrap_or(0);
        let val_b = b.get(i).copied().unwrap_or(0);
        if val_a != val_b {
            return val_a > val_b;
        }
    }
    false
}

/// Matches the running OS and CPU architecture against the released assets.
pub fn select_platform_asset(assets: &[ReleaseAsset]) -> Option<ReleaseAsset> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    match os {
        "windows" => {
            let is_arm = arch == "aarch64" || arch == "arm64";
            if is_arm {
                if let Some(a) = assets.iter().find(|a| {
                    a.name.to_lowercase().contains("arm64") && a.name.ends_with("-setup.exe")
                }) {
                    return Some(a.clone());
                }
                if let Some(a) = assets
                    .iter()
                    .find(|a| a.name.to_lowercase().contains("arm64") && a.name.ends_with(".msi"))
                {
                    return Some(a.clone());
                }
            } else {
                if let Some(a) = assets.iter().find(|a| {
                    (a.name.to_lowercase().contains("x64")
                        || a.name.to_lowercase().contains("x86_64"))
                        && a.name.ends_with("-setup.exe")
                }) {
                    return Some(a.clone());
                }
                if let Some(a) = assets.iter().find(|a| {
                    (a.name.to_lowercase().contains("x64")
                        || a.name.to_lowercase().contains("x86_64"))
                        && a.name.ends_with(".msi")
                }) {
                    return Some(a.clone());
                }
            }
            if let Some(a) = assets.iter().find(|a| a.name.ends_with("-setup.exe")) {
                return Some(a.clone());
            }
            if let Some(a) = assets.iter().find(|a| a.name.ends_with(".msi")) {
                return Some(a.clone());
            }
            if let Some(a) = assets.iter().find(|a| a.name.ends_with(".exe")) {
                return Some(a.clone());
            }
        }
        "macos" => {
            if let Some(a) = assets
                .iter()
                .find(|a| a.name.to_lowercase().contains("universal") && a.name.ends_with(".dmg"))
            {
                return Some(a.clone());
            }
            if let Some(a) = assets
                .iter()
                .find(|a| a.name.to_lowercase().contains(arch) && a.name.ends_with(".dmg"))
            {
                return Some(a.clone());
            }
            if let Some(a) = assets.iter().find(|a| a.name.ends_with(".dmg")) {
                return Some(a.clone());
            }
        }
        "linux" => {
            if let Some(a) = assets.iter().find(|a| a.name.ends_with(".AppImage")) {
                return Some(a.clone());
            }
            if let Some(a) = assets.iter().find(|a| a.name.ends_with(".deb")) {
                return Some(a.clone());
            }
            if let Some(a) = assets.iter().find(|a| a.name.ends_with(".rpm")) {
                return Some(a.clone());
            }
        }
        _ => {}
    }
    None
}

static STATUS: OnceLock<Mutex<UpdateStatus>> = OnceLock::new();
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

fn status_slot() -> &'static Mutex<UpdateStatus> {
    STATUS.get_or_init(|| Mutex::new(UpdateStatus::default()))
}

pub fn get_status() -> UpdateStatus {
    status_slot().lock().unwrap().clone()
}

/// Every network call the updater makes is bounded. Without a timeout a
/// stalled connection left the download flag set forever, and every later
/// attempt was refused as "busy" until the process restarted.
const METADATA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Browser URL of the release's `SHA256SUMS` asset, captured during the
/// metadata check. `None` when the release does not publish one.
static SUMS_URL: Mutex<Option<String>> = Mutex::new(None);

/// Checks GitHub for the latest release metadata.
pub fn check_for_update() -> Result<UpdateInfo, String> {
    let current_version = env!("CARGO_PKG_VERSION");
    let url = "https://api.github.com/repos/yjid/kensaku/releases/latest";

    let resp: GitHubRelease = ureq::get(url)
        .set(
            "User-Agent",
            &format!("kensaku-updater/{}", current_version),
        )
        .timeout(METADATA_TIMEOUT)
        .call()
        .map_err(|e| format!("Failed to reach GitHub release API: {e}"))?
        .into_json()
        .map_err(|e| format!("Failed to parse release information: {e}"))?;

    if let Ok(mut slot) = SUMS_URL.lock() {
        *slot = resp
            .assets
            .iter()
            .find(|asset| asset.name.eq_ignore_ascii_case("SHA256SUMS"))
            .map(|asset| asset.browser_download_url.clone());
    }

    let latest_tag = resp.tag_name.trim_start_matches('v');
    let has_update = is_newer_version(latest_tag, current_version);
    let chosen_asset = select_platform_asset(&resp.assets);

    Ok(UpdateInfo {
        current_version: current_version.to_string(),
        latest_version: latest_tag.to_string(),
        has_update,
        release_name: resp.name.unwrap_or_else(|| resp.tag_name.clone()),
        release_notes: resp.body.unwrap_or_default(),
        release_url: resp.html_url,
        published_at: resp.published_at.unwrap_or_default(),
        asset_name: chosen_asset.as_ref().map(|a| a.name.clone()),
        asset_url: chosen_asset
            .as_ref()
            .map(|a| a.browser_download_url.clone()),
        asset_size: chosen_asset.as_ref().map(|a| a.size),
    })
}

/// True when this install is the portable build (a `portable` marker file
/// sits next to the executable).
///
/// A portable install must not run the installer: it would place a second
/// copy in Program Files while the portable folder - and the index inside it -
/// stayed exactly as it was, so the user is told they updated and nothing
/// they are running actually changed.
pub fn is_portable_install() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    dir.join("portable").is_file() || dir.join("kensaku.portable").is_file()
}

/// Expected SHA-256 for `asset_name` from the release's `SHA256SUMS`.
///
/// Returns `Ok(None)` when the release publishes no checksum file or does not
/// list this asset: an older tag must still be installable.
fn expected_sha256(asset_name: &str) -> Result<Option<String>, String> {
    let url = match SUMS_URL.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    let Some(url) = url else {
        return Ok(None);
    };

    let body = ureq::get(&url)
        .set(
            "User-Agent",
            &format!("kensaku-updater/{}", env!("CARGO_PKG_VERSION")),
        )
        .timeout(METADATA_TIMEOUT)
        .call()
        .map_err(|e| format!("Could not fetch checksums: {e}"))?
        .into_string()
        .map_err(|e| format!("Could not read checksums: {e}"))?;

    for line in body.lines() {
        // "<sha256>  <filename>" (binary mode) or "<sha256> <filename>".
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*');
        if name == asset_name {
            return Ok(Some(hash.to_ascii_lowercase()));
        }
    }
    Ok(None)
}

fn sha256_of(path: &std::path::Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Could not open download: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| format!("Could not read download: {e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for &byte in digest.as_slice() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{:02x}", byte);
    }
    Ok(hex)
}

/// Directory that holds staged update downloads.
///
/// This lives under Kensaku's own data directory rather than the system temp
/// directory. The old code used a fixed `/tmp/kensaku-updates` path, so any
/// local user could pre-create it, own it, and control what the updater wrote
/// or executed.
fn update_staging_root() -> PathBuf {
    KensakuConfig::config_dir().join("updates")
}

/// Rejects asset names that are not a single plain file name.
///
/// The name comes from the GitHub release JSON and is joined onto a local
/// directory before the file is executed. A separator, a parent component or a
/// control character means the release metadata is malformed; refusing the
/// update is safer than guessing at the intended file.
fn validate_asset_name(asset_name: &str) -> Result<(), String> {
    if asset_name.is_empty() {
        return Err("Update asset name is empty".to_string());
    }
    if asset_name.len() > 255 {
        return Err("Update asset name is too long".to_string());
    }
    if asset_name == "." || asset_name == ".." {
        return Err(format!("Invalid update asset name: {asset_name}"));
    }
    if asset_name
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(format!("Invalid update asset name: {asset_name}"));
    }
    Ok(())
}

/// Creates `path` if needed and guarantees it is a private directory we own.
///
/// `create_dir_all` alone is not enough: it succeeds on an existing directory
/// (including one created by another user) and follows symlinks. Both cases
/// let an attacker choose where the updater writes. Every caller must run this
/// before using a staging directory.
fn ensure_private_staging_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|e| format!("Could not create update directory {path:?}: {e}"))?;

    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("Could not inspect update directory {path:?}: {e}"))?;
    if !meta.file_type().is_dir() {
        return Err(format!(
            "Update directory {path:?} is not a real directory (symlink or file)"
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // `libc::geteuid` is always safe; no pointer arguments.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(format!(
                "Update directory {path:?} is owned by uid {}, not the current user (uid {euid})",
                meta.uid()
            ));
        }

        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("Could not restrict update directory permissions: {e}"))?;
        }
    }

    Ok(())
}

/// Creates a fresh, unpredictable staging directory under the staging root.
///
/// `create_dir` (not `create_dir_all`) is atomic and fails if the name already
/// exists, so a pre-created directory can never be reused. The name is seeded
/// from the OS-random `RandomState`, plus pid and time, so it is not
/// predictable from the outside.
fn create_staging_dir(root: &Path) -> Result<PathBuf, String> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    ensure_private_staging_dir(root)?;

    for attempt in 0..16u32 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.write_u32(attempt);
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            hasher.write_u128(now.as_nanos());
        }
        let name = format!("{:016x}", hasher.finish());
        let candidate = root.join(name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                ensure_private_staging_dir(&candidate)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "Could not create update staging directory {candidate:?}: {e}"
                ));
            }
        }
    }

    Err("Could not allocate a unique update staging directory".to_string())
}

/// Re-validates a downloaded file immediately before it is executed.
///
/// The path is untrusted by then: it may have been replaced or pointed at a
/// symlink after the download finished. Require a regular file that lives in a
/// direct child of our private staging root, owned by the current user and not
/// writable by anyone else.
fn validate_staged_update(path: &Path) -> Result<(), String> {
    let root = update_staging_root();
    let parent = path
        .parent()
        .ok_or_else(|| format!("Invalid update path: {path:?}"))?;
    if parent.parent() != Some(root.as_path()) {
        return Err(format!(
            "Refusing to install an update outside the staging directory: {path:?}"
        ));
    }
    ensure_private_staging_dir(parent)?;

    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("Downloaded update file does not exist at {path:?}: {e}"))?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "Refusing to install an update that is not a regular file: {path:?}"
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(format!(
                "Refusing to install an update owned by uid {}, not the current user (uid {euid})",
                meta.uid()
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!(
                "Refusing to install a world/group-writable update: {path:?}"
            ));
        }
    }

    Ok(())
}

/// Initiates download of the chosen asset in a background worker thread.
pub fn download_update(asset_url: &str, asset_name: &str) -> Result<UpdateStatus, String> {
    if is_portable_install() {
        return Err(
            "This is a portable install: update by replacing the folder with the new portable zip. \
             In-app updates are disabled so the running copy is never silently replaced by a \
             second install."
                .to_string(),
        );
    }

    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Ok(get_status());
    }

    if let Err(err) = validate_asset_name(asset_name) {
        DOWNLOADING.store(false, Ordering::SeqCst);
        return Err(err);
    }

    let staging_dir = match create_staging_dir(&update_staging_root()) {
        Ok(dir) => dir,
        Err(e) => {
            DOWNLOADING.store(false, Ordering::SeqCst);
            let mut s = status_slot().lock().unwrap();
            s.busy = false;
            s.error = Some(format!("Could not create update directory: {e}"));
            return Ok(s.clone());
        }
    };

    let final_path = staging_dir.join(asset_name);
    let part_path = staging_dir.join(format!("{}.part", asset_name));

    // Reset status
    {
        let mut s = status_slot().lock().unwrap();
        s.busy = true;
        s.error = None;
        s.ready_to_install = false;
        s.downloaded_path = None;
        s.progress = Some(UpdateProgress {
            phase: "downloading".to_string(),
            downloaded: 0,
            total: 0,
            percent: 0,
        });
    }

    let url = asset_url.to_string();
    let asset_name_for_check = asset_name.to_string();

    std::thread::Builder::new()
        .name("kensaku-updater".to_string())
        .spawn(move || {
            let res = (|| -> Result<(), String> {
                let resp = ureq::get(&url)
                    .set(
                        "User-Agent",
                        &format!("kensaku-updater/{}", env!("CARGO_PKG_VERSION")),
                    )
                    .timeout(DOWNLOAD_TIMEOUT)
                    .call()
                    .map_err(|e| format!("Download request failed: {e}"))?;

                let total_bytes: u64 = resp
                    .header("Content-Length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);

                let mut reader = resp.into_reader();
                let mut file = std::fs::File::create(&part_path)
                    .map_err(|e| format!("Failed to create temporary file: {e}"))?;

                let mut downloaded: u64 = 0;
                let mut buf = [0u8; 64 * 1024];

                loop {
                    use std::io::Read;
                    use std::io::Write;
                    let n = reader
                        .read(&mut buf)
                        .map_err(|e| format!("Read error: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    file.write_all(&buf[..n])
                        .map_err(|e| format!("Write error: {e}"))?;
                    downloaded += n as u64;

                    let percent = if total_bytes > 0 {
                        ((downloaded as f64 / total_bytes as f64) * 100.0).min(100.0) as u8
                    } else {
                        0
                    };

                    let mut s = status_slot().lock().unwrap();
                    s.progress = Some(UpdateProgress {
                        phase: "downloading".to_string(),
                        downloaded,
                        total: total_bytes,
                        percent,
                    });
                }

                // A body that ends early reads as a clean EOF, so the loop
                // above cannot tell a finished download from a dropped
                // connection. Renaming the .part file unconditionally marked
                // a truncated installer as ready and the user ran it.
                if total_bytes > 0 && downloaded != total_bytes {
                    let _ = std::fs::remove_file(&part_path);
                    return Err(format!(
                        "Download incomplete: got {downloaded} of {total_bytes} bytes"
                    ));
                }

                // Verify against the release's published checksum before the
                // file is ever executable. The workflow already publishes
                // SHA256SUMS; the updater used to ignore it entirely.
                if let Some(expected) = expected_sha256(&asset_name_for_check)? {
                    let actual = sha256_of(&part_path)?;
                    if actual != expected {
                        let _ = std::fs::remove_file(&part_path);
                        return Err(format!(
                            "Update failed verification: expected {expected}, got {actual}"
                        ));
                    }
                }

                let _ = std::fs::remove_file(&final_path);
                std::fs::rename(&part_path, &final_path)
                    .map_err(|e| format!("Failed to finalize update file: {e}"))?;

                Ok(())
            })();

            let mut s = status_slot().lock().unwrap();
            s.busy = false;
            DOWNLOADING.store(false, Ordering::SeqCst);
            match res {
                Ok(()) => {
                    s.ready_to_install = true;
                    s.downloaded_path = Some(final_path.to_string_lossy().to_string());
                    if let Some(ref mut p) = s.progress {
                        p.phase = "ready".to_string();
                        p.percent = 100;
                    }
                }
                Err(err) => {
                    s.error = Some(err);
                    if let Some(ref mut p) = s.progress {
                        p.phase = "error".to_string();
                    }
                }
            }
        })
        .map_err(|e| format!("Failed to spawn updater thread: {e}"))?;

    Ok(get_status())
}

/// Spawns the installer detached and exits Kensaku so files are replaced cleanly.
pub fn install_and_restart(app: &AppHandle) -> Result<(), String> {
    let path_str = {
        let s = status_slot().lock().unwrap();
        s.downloaded_path
            .clone()
            .ok_or_else(|| "No update has been downloaded yet".to_string())?
    };

    let path = PathBuf::from(&path_str);
    validate_staged_update(&path)?;

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const DETACHED_PROCESS: u32 = 0x00000008;

        if path_str.ends_with(".msi") {
            let mut cmd = std::process::Command::new("msiexec");
            cmd.args(["/i", &path_str]);
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
            cmd.spawn()
                .map_err(|e| format!("Failed to launch MSI installer: {e}"))?;
        } else {
            let mut cmd = std::process::Command::new(&path);
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
            cmd.spawn()
                .map_err(|e| format!("Failed to launch installer: {e}"))?;
        }
        app.exit(0);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let _ = app;
        std::process::Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to open DMG installer: {e}"))?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        if path_str.ends_with(".AppImage") {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| e.to_string())?
                .permissions();
            perms.set_mode(0o755);
            let _ = std::fs::set_permissions(&path, perms);
            std::process::Command::new(&path)
                .spawn()
                .map_err(|e| format!("Failed to launch AppImage: {e}"))?;
            app.exit(0);
            Ok(())
        } else {
            let _ = app;
            std::process::Command::new("xdg-open")
                .arg(&path)
                .spawn()
                .map_err(|e| format!("Failed to launch package installer: {e}"))?;
            Ok(())
        }
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = app;
        Err("Unsupported operating system for in-app update launch".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("1.0.14", "1.0.13"));
        assert!(is_newer_version("v1.0.14", "1.0.13"));
        assert!(is_newer_version("1.0.14-beta.1", "1.0.13"));
        assert!(is_newer_version("1.1.0", "1.0.13"));
        assert!(is_newer_version("2.0.0", "1.0.13"));

        assert!(!is_newer_version("1.0.13", "1.0.13"));
        assert!(!is_newer_version("v1.0.13", "1.0.13"));
        assert!(!is_newer_version("1.0.12", "1.0.13"));
        assert!(!is_newer_version("0.9.9", "1.0.13"));
    }

    #[test]
    fn test_select_platform_asset() {
        let assets = vec![
            ReleaseAsset {
                name: "kensaku_1.0.14_x64-setup.exe".to_string(),
                browser_download_url: "http://example.com/setup.exe".to_string(),
                size: 1000,
            },
            ReleaseAsset {
                name: "kensaku_1.0.14_arm64-setup.exe".to_string(),
                browser_download_url: "http://example.com/arm-setup.exe".to_string(),
                size: 1000,
            },
            ReleaseAsset {
                name: "kensaku_1.0.14_universal.dmg".to_string(),
                browser_download_url: "http://example.com/kensaku.dmg".to_string(),
                size: 1000,
            },
            ReleaseAsset {
                name: "kensaku_1.0.14_amd64.AppImage".to_string(),
                browser_download_url: "http://example.com/kensaku.AppImage".to_string(),
                size: 1000,
            },
        ];

        let selected = select_platform_asset(&assets);
        assert!(selected.is_some());
        let name = selected.unwrap().name;
        if cfg!(windows) {
            if cfg!(target_arch = "aarch64") {
                assert!(name.contains("arm64"));
            } else {
                assert!(name.contains("x64"));
            }
        } else if cfg!(target_os = "macos") {
            assert!(name.ends_with(".dmg"));
        } else if cfg!(target_os = "linux") {
            assert!(name.ends_with(".AppImage"));
        }
    }

    fn temp_test_root(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "kensaku-updater-test-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn asset_name_must_be_a_plain_file_name() {
        for ok in ["kensaku_1.0.17_x64-setup.exe", "kensaku.AppImage", "a"] {
            assert!(validate_asset_name(ok).is_ok(), "{ok} should be accepted");
        }
        for bad in [
            "",
            ".",
            "..",
            "../evil",
            "dir/evil",
            "dir\\evil",
            "/abs/path",
            "with\nnewline",
            "with\0nul",
        ] {
            assert!(
                validate_asset_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn staging_dirs_are_unique_and_private() {
        let root = temp_test_root("unique");
        let first = create_staging_dir(&root).expect("first staging dir");
        let second = create_staging_dir(&root).expect("second staging dir");

        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&first, &second] {
                let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode & 0o077, 0, "staging dir {dir:?} must be private");
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn staging_dir_rejects_a_symlink() {
        let root = temp_test_root("symlink");
        std::fs::create_dir_all(&root).unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();

        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(
                ensure_private_staging_dir(&link).is_err(),
                "a symlinked staging dir must be refused"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn staged_update_must_live_in_the_private_staging_root() {
        let outside = temp_test_root("outside").join("installer.AppImage");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, b"not an installer").unwrap();
        assert!(
            validate_staged_update(&outside).is_err(),
            "files outside the staging root must never be executed"
        );
        let _ = std::fs::remove_dir_all(outside.parent().unwrap());
    }
}
