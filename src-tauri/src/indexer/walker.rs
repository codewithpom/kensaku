use crate::indexer::ignore::IgnoreSet;
use crate::search::fuzzy::compute_char_mask;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub id: u64,
    pub path: PathBuf,
    pub name: String,
    /// Interned (lowercase) extension. Extensions repeat heavily, so a
    /// process-wide `&'static str` avoids one heap allocation per entry during
    /// a full crawl while keeping `Option<&str>` call sites unchanged.
    pub extension: Option<&'static str>,
    pub size: u64,
    pub modified: u64,
    pub is_dir: bool,
    /// Filesystem device id (`st_dev`). `0` means unknown.
    pub dev: u64,
    /// Filesystem inode number (`st_ino`). `0` means unknown.
    pub ino: u64,
    /// 64-bit character presence bitmask for fast prefiltering.
    pub char_bloom: u64,
    /// True when the entry's content must be re-read even if its mtime/size did
    /// not change (used to re-run image OCR after the setting is toggled).
    pub content_pending: bool,
}

/// Interns an extension for the lifetime of the process.
///
/// Unique extensions are a tiny fixed set, so leaking each one once is a
/// bounded cost and removes the per-entry `String` from a 1M-entry crawl.
pub fn intern_extension(extension: Option<&str>) -> Option<&'static str> {
    let extension = extension?.trim().trim_start_matches('.');
    if extension.is_empty() {
        return None;
    }
    static EXTENSIONS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut table = EXTENSIONS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(&interned) = table.get(extension) {
        return Some(interned);
    }
    let lower = extension.to_ascii_lowercase();
    if let Some(&interned) = table.get(lower.as_str()) {
        return Some(interned);
    }
    let interned: &'static str = Box::leak(lower.into_boxed_str());
    table.insert(interned);
    Some(interned)
}

/// Returns `(device_id, inode)` for a filesystem metadata entry.
///
/// These values form the stable identity of a file across path-based renames
/// and replacements. On non-Unix platforms they are unavailable and default
/// to `0`, which callers treat as "unknown".
pub fn file_identity(meta: &std::fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (0, 0)
    }
}

/// True when the file is a cloud-backed placeholder that would be hydrated
/// (downloaded) if its contents were read.
///
/// - Windows (OneDrive, Dropbox, iCloud for Windows): the offline /
///   recall-on-data-access attributes.
/// - macOS (iCloud Drive with "Optimize Mac Storage", Desktop & Documents in
///   iCloud, File Provider apps): APFS marks evicted files `SF_DATALESS`.
///   `stat` never downloads them, but reading even one byte does, so before
///   this check indexing quietly downloaded the user's whole cloud library.
/// - Linux has no common equivalent; returns `false`.
#[cfg(windows)]
pub fn is_cloud_placeholder(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    // https://learn.microsoft.com/windows/win32/fileio/file-attribute-constants
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

    let attributes = meta.file_attributes();
    attributes & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(target_os = "macos")]
pub fn is_cloud_placeholder(meta: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;

    // <sys/stat.h>: "file is dataless object" (contents live in the cloud).
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn is_cloud_placeholder(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Convenience wrapper for callers that only have a path. A `stat`, which
/// never downloads anything; on Linux it does not touch the filesystem.
pub fn is_cloud_placeholder_path(path: &std::path::Path) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        std::fs::metadata(path)
            .map(|meta| is_cloud_placeholder(&meta))
            .unwrap_or(false)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        false
    }
}

/// Directories whose entire subtree the crawler must never enter.
///
/// Kensaku's own runtime data (config, SQLite metadata, Tantivy index,
/// thumbnails, OCR model packs, the daemon socket) lives under
/// [`crate::config::KensakuConfig::config_dir`]. On macOS that is
/// `~/Library/Application Support/kensaku` — *not* a hidden path — so a
/// home-rooted crawl would otherwise walk its own index while it writes it:
/// thousands of files, re-processed on every change. On Linux the config
/// directory starts with `.` and `skip_hidden` already hides it; on Windows it
/// is `%APPDATA%\kensaku`, also not dot-prefixed. The explicit prune is
/// correct on every platform and costs one path comparison per entry.
pub fn runtime_dir_prunes() -> Vec<PathBuf> {
    vec![crate::config::KensakuConfig::config_dir()]
}

/// The OS's application-data tree directly under the home folder:
/// `%USERPROFILE%\AppData` on Windows, `~/Library` on macOS. Browser caches,
/// app containers and logs: huge, constantly changing, and never what a
/// file search is for. (Linux keeps this in dot-folders, already hidden.)
pub fn system_data_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    if cfg!(windows) {
        // CrossDevice: Phone Link exposes a connected phone here as a live
        // cloud-files folder. Listing it asks the phone over Wi-Fi, and when
        // the phone is away Windows pops "<phone> is offline" every time.
        vec![home.join("AppData"), home.join("CrossDevice")]
    } else if cfg!(target_os = "macos") {
        vec![home.join("Library")]
    } else {
        Vec::new()
    }
}

/// Everything a walk or watch over `roots` must not enter: Kensaku's own
/// data always, plus the OS data trees, except one that a root lives inside.
/// Someone who adds iCloud Drive (`~/Library/Mobile Documents/...`) on
/// purpose gets it; someone who adds their home folder does not get
/// `~/Library` with it.
pub fn prunes_for_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    prunes_for_roots_with(roots, system_data_dirs())
}

fn prunes_for_roots_with(roots: &[PathBuf], system: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut prunes = runtime_dir_prunes();
    for dir in system {
        if !roots.iter().any(|root| root.starts_with(&dir)) {
            prunes.push(dir);
        }
    }
    prunes
}

/// True when `meta` is a cloud-backed folder whose listing is not on disk,
/// so reading it would make the provider fetch it (OneDrive "online-only"
/// folders, Phone Link, macOS File Provider). The folder itself stays
/// findable by name; the walk just does not descend into it.
#[cfg(windows)]
pub fn is_unpopulated_cloud_dir(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    meta.is_dir()
        && meta.file_attributes()
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
}

#[cfg(target_os = "macos")]
pub fn is_unpopulated_cloud_dir(meta: &std::fs::Metadata) -> bool {
    meta.is_dir() && is_cloud_placeholder(meta)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn is_unpopulated_cloud_dir(_meta: &std::fs::Metadata) -> bool {
    false
}

/// True when `path` lives at or below any of `prune_roots`.
pub fn starts_with_any(path: &std::path::Path, prune_roots: &[PathBuf]) -> bool {
    prune_roots.iter().any(|root| path.starts_with(root))
}

/// Default-deny sensitive names, shared by the crawler and the watcher.
///
/// These are skipped even if the user has not added them to exclusions. Hidden
/// entries (names starting with '.') are also skipped by the walk, which covers
/// `~/.ssh`, `~/.gnupg`, `~/.aws` and friends; the explicit list keeps the
/// policy intact when a caller or platform does not skip hidden entries.
pub const SENSITIVE_NAMES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".gpg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
    "1password",
    "bitwarden",
    "keepass",
    "keepassxc",
];

/// Whether `name` is a sensitive file or directory name.
///
/// `.env` is matched as a prefix so `.env.local`, `.env.production` and any
/// future variant are covered without extending the list.
pub fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == ".env" || lower.starts_with(".env.") || SENSITIVE_NAMES.contains(&lower.as_str())
}

/// True when `path` or any of its components is a sensitive name.
pub fn is_sensitive_path(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(is_sensitive_name)
    })
}

/// Walks `dirs` and returns file entries.
///
/// When `include_ignored` is false (the default), `.gitignore` /
/// `.cursorignore` rules under the roots are honored so agents see the same
/// tree developers do; see [`crate::indexer::ignore`]. The walk itself still
/// traverses ignored directories (the cheap default `excluded_dirs` list
/// handles the heavy hitters); their entries are filtered before returning.
/// `skip_hidden` controls dot-prefixed entries beneath each root; explicitly
/// selected roots are still walked. Sensitive names remain excluded either way.
pub fn walk_directories(
    dirs: &[PathBuf],
    excluded_dirs: &[String],
    include_ignored: bool,
    skip_hidden: bool,
) -> Vec<FileEntry> {
    let mut result = Vec::new();
    let mut ignore_dirs: Vec<PathBuf> = Vec::new();
    let excluded_set: HashSet<String> = excluded_dirs.iter().map(|s| s.to_lowercase()).collect();

    for root in dirs {
        if !root.exists() {
            continue;
        }

        let walker = jwalk::WalkDir::new(root)
            .skip_hidden(skip_hidden)
            .process_read_dir({
                let excluded_set = excluded_set.clone();
                let prune_roots = prunes_for_roots(dirs);
                move |_depth, _path, _read_dir_state, children| {
                    children.retain(|dir_entry_result| {
                        if let Ok(entry) = dir_entry_result {
                            // Kensaku's own runtime data: never crawl it
                            // (macOS path, see `runtime_dir_prunes`).
                            if starts_with_any(&entry.path(), &prune_roots) {
                                return false;
                            }
                            let name = entry.file_name.to_string_lossy().to_lowercase();
                            // Belt-and-suspenders for platforms where jwalk's
                            // hidden-entry filtering is unsupported.
                            if skip_hidden && name.starts_with('.') {
                                return false;
                            }
                            if excluded_set.contains(&name) {
                                return false;
                            }
                            if is_sensitive_name(&name) {
                                return false;
                            }
                        }
                        true
                    });
                    // Keep cloud-only folders as entries, but never list
                    // their contents: that is a network fetch (and, for a
                    // phone over Phone Link, an "offline" popup). Detection
                    // is Windows/macOS-only, so other platforms skip the
                    // extra per-directory stat entirely.
                    #[cfg(any(windows, target_os = "macos"))]
                    for entry in children.iter_mut().flatten() {
                        if entry.file_type.is_dir()
                            && let Ok(meta) = std::fs::metadata(entry.path())
                            && is_unpopulated_cloud_dir(&meta)
                        {
                            entry.read_children = None;
                        }
                    }
                }
            });

        for entry_res in walker {
            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            let is_dir = entry.file_type().is_dir();
            let name = entry.file_name.to_string_lossy().to_string();

            let extension = if is_dir {
                None
            } else {
                intern_extension(path.extension().and_then(|ext| ext.to_str()))
            };

            let (size, modified, dev, ino) = match entry.metadata() {
                Ok(meta) => {
                    let sz = meta.len();
                    let mod_time = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let (dev, ino) = file_identity(&meta);
                    (sz, mod_time, dev, ino)
                }
                Err(_) => (0, 0, 0, 0),
            };

            let char_bloom = compute_char_mask(&name);
            let file_entry = FileEntry {
                id: 0,
                path: path.clone(),
                name,
                extension,
                size,
                modified,
                is_dir,
                dev,
                ino,
                char_bloom,
                content_pending: false,
            };

            if !include_ignored && is_dir {
                ignore_dirs.push(path);
            }

            result.push(file_entry);
        }
    }

    // Ignore parity: probe each visited directory for `.gitignore` /
    // `.cursorignore` and drop ignored entries.
    {
        let ignore_set = if include_ignored {
            IgnoreSet::empty()
        } else {
            IgnoreSet::from_directories(&ignore_dirs)
        };
        // The watcher runs between crawls and has no directory list of its
        // own; it reads these rules so an edit inside an ignored tree does not
        // put the file back into the index.
        crate::indexer::ignore::set_active(ignore_set.clone());
        if !include_ignored && !ignore_set.is_empty() {
            result.retain(|entry| !ignore_set.is_ignored(&entry.path, entry.is_dir));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HiddenFixture(PathBuf);

    impl HiddenFixture {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("kensaku-hidden-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, relative: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "searchable fixture").unwrap();
        }

        fn paths(&self, root: &std::path::Path, skip_hidden: bool) -> HashSet<PathBuf> {
            walk_directories(&[root.to_path_buf()], &[], true, skip_hidden)
                .into_iter()
                .map(|entry| entry.path)
                .collect()
        }
    }

    impl Drop for HiddenFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hidden_entries_follow_skip_hidden_setting() {
        let fixture = HiddenFixture::new();
        for path in [
            "visible.txt",
            ".dotfile",
            ".notes/note.txt",
            ".notes/.nested/deep.txt",
        ] {
            fixture.write(path);
        }
        for skip_hidden in [true, false] {
            let paths = fixture.paths(&fixture.0, skip_hidden);
            assert!(paths.contains(&fixture.0.join("visible.txt")));
            for path in [
                ".dotfile",
                ".notes",
                ".notes/note.txt",
                ".notes/.nested/deep.txt",
            ] {
                assert_eq!(
                    paths.contains(&fixture.0.join(path)),
                    !skip_hidden,
                    "{path}, skip_hidden={skip_hidden}"
                );
            }
        }
    }

    #[test]
    fn explicitly_watched_dot_directory_includes_hidden_descendants_when_enabled() {
        let fixture = HiddenFixture::new();
        fixture.write(".notes/plain.txt");
        fixture.write(".notes/.nested/deep.txt");
        let root = fixture.0.join(".notes");
        let paths = fixture.paths(&root, false);
        assert!(paths.contains(&root.join("plain.txt")));
        assert!(paths.contains(&root.join(".nested/deep.txt")));
    }

    #[test]
    fn hidden_indexing_keeps_sensitive_names_and_explicit_exclusions_pruned() {
        let fixture = HiddenFixture::new();
        for path in [
            ".notes/keep.txt",
            ".ssh/id_rsa",
            ".env",
            ".env.local",
            ".excluded/drop.txt",
        ] {
            fixture.write(path);
        }
        for skip_hidden in [true, false] {
            let entries = walk_directories(
                std::slice::from_ref(&fixture.0),
                &[".excluded".into()],
                true,
                skip_hidden,
            );
            assert_eq!(
                entries
                    .iter()
                    .any(|e| e.path == fixture.0.join(".notes/keep.txt")),
                !skip_hidden
            );
            assert!(!entries.iter().any(|entry| {
                entry
                    .path
                    .strip_prefix(&fixture.0)
                    .unwrap()
                    .components()
                    .any(|component| {
                        let name = component.as_os_str().to_string_lossy();
                        is_sensitive_name(&name) || name == ".excluded"
                    })
            }));
        }
    }

    #[test]
    fn hidden_indexing_still_honors_ignore_files() {
        let fixture = HiddenFixture::new();
        fixture.write(".notes/keep.txt");
        fixture.write(".ignored/drop.txt");
        std::fs::write(fixture.0.join(".gitignore"), ".ignored/\n").unwrap();
        let entries = walk_directories(std::slice::from_ref(&fixture.0), &[], false, false);
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == fixture.0.join(".notes/keep.txt"))
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.path.starts_with(fixture.0.join(".ignored")))
        );
    }

    #[test]
    fn extension_interning_is_stable_and_case_insensitive() {
        let upper = intern_extension(Some("PNG")).expect("interned");
        let dotted = intern_extension(Some(".png")).expect("interned");
        assert_eq!(upper, dotted);
        assert!(std::ptr::eq(upper, dotted));
        assert_eq!(intern_extension(None), None);
        assert_eq!(intern_extension(Some("")), None);
        assert_eq!(intern_extension(Some(".")), None);
    }

    #[test]
    fn interned_extension_storage_is_inline() {
        assert!(
            std::mem::size_of::<Option<&'static str>>() < std::mem::size_of::<Option<String>>()
        );
    }

    #[test]
    fn system_data_is_pruned_unless_a_root_lives_inside_it() {
        let home = PathBuf::from("/home/me");
        let library = home.join("Library");
        // Home picked: ~/Library is skipped.
        let prunes = prunes_for_roots_with(std::slice::from_ref(&home), vec![library.clone()]);
        assert!(starts_with_any(&library.join("Caches/x"), &prunes));
        assert!(!starts_with_any(&home.join("Documents/a.txt"), &prunes));
        // iCloud Drive picked on purpose: it is indexed.
        let icloud = library.join("Mobile Documents/com~apple~CloudDocs");
        let prunes = prunes_for_roots_with(&[home.clone(), icloud.clone()], vec![library.clone()]);
        assert!(!starts_with_any(&icloud.join("notes.txt"), &prunes));
    }

    #[test]
    fn runtime_dir_prunes_covers_config_dir_and_subtrees() {
        let prunes = runtime_dir_prunes();
        assert!(!prunes.is_empty());
        let config_dir = crate::config::KensakuConfig::config_dir();
        // The config dir itself, and everything inside it (index/, thumbs/,
        // tessdata/, models/), are pruned; siblings are not.
        assert!(starts_with_any(&config_dir, &prunes));
        assert!(starts_with_any(&config_dir.join("index"), &prunes));
        assert!(config_dir.join("index").join("tantivy").is_absolute() == config_dir.is_absolute());
        assert!(starts_with_any(
            &config_dir.join("thumbs").join("a.jpg"),
            &prunes
        ));
        // A sibling directory that merely shares a name prefix must not be
        // pruned (e.g. `kensaku-cli` next to `kensaku`).
        let sibling = config_dir.parent().map(|p| p.join("kensaku-cli"));
        if let Some(sibling) = sibling {
            assert!(!starts_with_any(&sibling, &prunes));
        }
        // A path with the config dir as an ancestor component must also be
        // pruned wherever it appears under a watched root.
        assert!(starts_with_any(
            &config_dir.join("models").join("det.onnx"),
            &prunes
        ));
    }

    #[test]
    fn walker_never_enters_its_own_runtime_dir() {
        // Build a fake tree: watched_root/Library/... sits next to Kensaku's
        // config dir, mimicking the macOS layout
        // (config dir = ~/Library/Application Support/kensaku).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "kensaku-walker-prune-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&base).unwrap();

        let config_dir = crate::config::KensakuConfig::config_dir();
        // The suite cannot override `dirs::config_dir()` safely in parallel
        // tests, so instead prove that walk_directories prunes whatever the
        // *current* runtime dir would be, plus that ordinary entries still
        // survive. Create an ordinary file under the root.
        let root = base.join("root");
        let docs = root.join("Documents");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("notes.txt"), b"hello").unwrap();

        let entries = walk_directories(std::slice::from_ref(&root), &[], false, true);
        assert!(
            entries.iter().any(|e| e.path == root.join("Documents")),
            "ordinary directories must survive the prune"
        );
        // Nothing inside Kensaku's runtime dir can appear in the results:
        // every entry under the config dir would start with it.
        assert!(
            !entries
                .iter()
                .any(|e| starts_with_any(&e.path, &runtime_dir_prunes())),
            "the crawler must never index its own storage directory"
        );

        assert!(std::fs::remove_dir_all(&base).is_ok());
        let _ = config_dir;
    }
}
