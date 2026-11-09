use crate::indexer::arena::{IdIndex, NO_PARENT, Node, NodeArena, PathIndex};
use crate::indexer::metadata_db::MetadataDb;
use crate::indexer::walker::FileEntry;
use crate::query::scope_trie::DirectoryTrie;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

pub struct MetadataStore {
    arena: NodeArena,
    path_index: PathIndex,
    id_index: RwLock<Option<IdIndex>>,
    /// Parallel to `arena`: burst id per arena slot, 0 means unassigned.
    bursts: RwLock<Option<Vec<u64>>>,
    db: Option<Arc<MetadataDb>>,
    generation: AtomicU64,
    dir_generation: AtomicU64,
    usage: RwLock<HashMap<PathBuf, (u32, u64)>>,
    cached_trie: RwLock<Option<(u64, Arc<DirectoryTrie>)>>,
    /// Materialized entries for the current generation.
    ///
    /// Building one is several allocations per file: the path is rebuilt from
    /// the arena, a PathBuf is joined, the name is copied. Search needs the
    /// whole list on every query, so it is cached here, per store, next to the
    /// trie cache and invalidated the same way. Per store rather than global
    /// because more than one store can exist in a process (the daemon, a
    /// headless agent index, a test fixture), and a global cache would hand one
    /// store's rows to another.
    cached_entries: RwLock<Option<(u64, Arc<Vec<crate::indexer::walker::FileEntry>>)>>,
}

impl Default for MetadataStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataStore {
    pub fn without_db() -> Self {
        Self {
            arena: NodeArena::new(),
            path_index: PathIndex::new(),
            id_index: RwLock::new(None),
            bursts: RwLock::new(None),
            db: None,
            generation: AtomicU64::new(1),
            dir_generation: AtomicU64::new(1),
            usage: RwLock::new(HashMap::new()),
            cached_trie: RwLock::new(None),
            cached_entries: RwLock::new(None),
        }
    }

    pub fn new() -> Self {
        let db = match MetadataDb::open() {
            Ok(db) => Some(Arc::new(db)),
            Err(e) => {
                eprintln!("[Kensaku] Failed to open metadata database: {}", e);
                None
            }
        };

        let mut store = Self::without_db();
        store.db = db;
        if store.db.is_some() {
            let loaded = store.load_from_db();
            if loaded > 0 {
                println!("[Kensaku] Loaded {} metadata entries from database", loaded);
            }
        }
        store
    }

    pub fn arena(&self) -> &NodeArena {
        &self.arena
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn with_db(db: MetadataDb) -> Self {
        let mut store = Self::without_db();
        store.db = Some(Arc::new(db));
        store.load_from_db();
        store
    }

    pub fn load_from_db(&mut self) -> usize {
        let db = match self.db.as_ref() {
            Some(db) => Arc::clone(db),
            None => return 0,
        };

        let entries = match db.load_all() {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("[Kensaku] Failed to load metadata from database: {}", e);
                return 0;
            }
        };

        if let Ok(loaded_usage) = db.load_usage() {
            *self.usage.write() = loaded_usage;
        }

        self.arena = NodeArena::new();
        self.path_index.clear();
        *self.id_index.write() = None;
        *self.bursts.write() = None;

        let count = entries.len();
        // First pass: push nodes (parents resolved later). The path index is
        // built last: its `grow` re-keys slots from `arena.path()`, which is only
        // the bare file name until parents are linked.
        let mut idxs = Vec::with_capacity(entries.len());
        let mut by_path: std::collections::HashMap<&Path, u32> =
            std::collections::HashMap::with_capacity(entries.len());
        for entry in &entries {
            let ext_id = self.arena.intern_ext(entry.extension);
            let (name_off, name_len) = self.arena.names.push(&entry.name);
            let mut flags = 0u8;
            if entry.is_dir {
                flags |= Node::FLAG_DIR;
            }
            if entry.content_pending {
                flags |= Node::FLAG_CONTENT_PENDING;
            }
            let node = Node {
                parent: NO_PARENT,
                name_off,
                name_len,
                ext_id,
                flags,
                size: entry.size,
                mtime: entry.modified,
                dev: entry.dev,
                ino: entry.ino,
                char_bloom: entry.char_bloom,
                node_id: entry.id,
            };
            let idx = self.arena.push(node);
            by_path.insert(entry.path.as_path(), idx);
            idxs.push(idx);
        }
        // Second pass: resolve parent pointers.
        for (entry, &idx) in entries.iter().zip(idxs.iter()) {
            let parent = entry
                .path
                .parent()
                .and_then(|parent| by_path.get(parent).copied())
                .unwrap_or(NO_PARENT);
            if parent == NO_PARENT {
                let full = entry.path.to_string_lossy().into_owned();
                let (off, len) = self.arena.names.push(&full);
                let node = self.arena.node_mut(idx);
                node.parent = parent;
                node.name_off = off;
                node.name_len = len;
            } else {
                let node = self.arena.node_mut(idx);
                node.parent = parent;
            }
        }
        // Third pass: every node now has its final name and parent.
        for (entry, &idx) in entries.iter().zip(idxs.iter()) {
            self.path_index.insert(&self.arena, &entry.path, idx);
        }

        if let Ok(loaded_bursts) = db.load_bursts() {
            if !loaded_bursts.is_empty() {
                let mut bursts = vec![0u64; self.arena.len()];
                for (node_id, burst) in loaded_bursts {
                    if let Some(idx) = self.get_index_by_node_id(node_id)
                        && (idx as usize) < bursts.len()
                    {
                        bursts[idx as usize] = burst;
                    }
                }
                *self.bursts.write() = Some(bursts);
            } else {
                *self.bursts.write() = None;
            }
        }

        self.generation.fetch_add(1, Ordering::SeqCst);
        self.dir_generation.fetch_add(1, Ordering::SeqCst);
        *self.cached_trie.write() = None;

        count
    }

    fn insert_path_index(&mut self, path: &Path, idx: u32) {
        self.path_index.insert(&self.arena, path, idx);
    }

    fn find_path_index(&self, path: &Path) -> Option<u32> {
        self.path_index.find(&self.arena, path)
    }

    pub fn clear(&mut self) {
        self.arena = NodeArena::new();
        self.path_index.clear();
        *self.id_index.write() = None;
        *self.bursts.write() = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.dir_generation.fetch_add(1, Ordering::SeqCst);
        *self.cached_trie.write() = None;

        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.clear()
        {
            eprintln!("[Kensaku] Failed to clear metadata database: {}", e);
        }
    }

    pub fn add(&mut self, entry: FileEntry) -> u64 {
        let is_dir = entry.is_dir;
        let mut entry_id = entry.id;
        // find existing
        let existing = self.find_path_index(&entry.path);
        let idx = if let Some(idx) = existing {
            // preserve content_pending
            let mut pending = self.arena.node(idx).content_pending();
            if self.arena.node(idx).content_pending() {
                pending = true;
            }
            // update node
            let ext_id = self.arena.intern_ext(entry.extension);
            let parent = self.parent_index_for(&entry.path);
            let node_name = if parent == NO_PARENT {
                entry.path.to_string_lossy().into_owned()
            } else {
                entry.name.clone()
            };
            let (name_off, name_len) = self.arena.names.push(&node_name);
            let mut flags = 0u8;
            if entry.is_dir {
                flags |= Node::FLAG_DIR;
            }
            if pending || entry.content_pending {
                flags |= Node::FLAG_CONTENT_PENDING;
            }
            let node = Node {
                parent,
                name_off,
                name_len,
                ext_id,
                flags,
                size: entry.size,
                mtime: entry.modified,
                dev: entry.dev,
                ino: entry.ino,
                char_bloom: entry.char_bloom,
                node_id: entry.id,
            };
            *self.arena.node_mut(idx) = node;
            idx
        } else {
            let ext_id = self.arena.intern_ext(entry.extension);
            let parent = self.parent_index_for(&entry.path);
            let name = if parent == NO_PARENT {
                entry.path.to_string_lossy().into_owned()
            } else {
                entry
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.name.clone())
            };
            let (name_off, name_len) = self.arena.names.push(&name);
            let mut flags = 0u8;
            if entry.is_dir {
                flags |= Node::FLAG_DIR;
            }
            if entry.content_pending {
                flags |= Node::FLAG_CONTENT_PENDING;
            }
            let node = Node {
                parent,
                name_off,
                name_len,
                ext_id,
                flags,
                size: entry.size,
                mtime: entry.modified,
                dev: entry.dev,
                ino: entry.ino,
                char_bloom: entry.char_bloom,
                node_id: entry.id,
            };
            let idx = self.arena.push(node);
            self.path_index.insert(&self.arena, &entry.path, idx);
            idx
        };

        self.generation.fetch_add(1, Ordering::SeqCst);

        if is_dir {
            self.dir_generation.fetch_add(1, Ordering::SeqCst);
            *self.cached_trie.write() = None;
        }

        if let Some(db) = self.db.as_ref() {
            // Need a FileEntry view for db upsert
            let view = self.materialize(idx);
            match db.upsert(&view) {
                Ok(id) => {
                    self.arena.node_mut(idx).node_id = id;
                    entry_id = id;
                    if let Some(index) = self.id_index.write().as_mut()
                        && id > 0
                    {
                        index.insert(id, idx);
                    }
                    if let Some(bursts) = self.bursts.write().as_mut()
                        && idx as usize >= bursts.len()
                    {
                        bursts.resize(self.arena.len(), 0);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[Kensaku] Failed to persist metadata entry '{}': {}",
                        view.path.display(),
                        e
                    );
                }
            }
        }

        entry_id
    }

    fn parent_index_for(&mut self, path: &Path) -> u32 {
        if let Some(parent) = path.parent()
            && let Some(idx) = self.find_path_index(parent)
        {
            return idx;
        }
        NO_PARENT
    }

    /// Marks every image and PDF for a content re-read. Used when what OCR
    /// produces has changed (an engine became usable, an OCR fix shipped).
    pub fn mark_ocr_targets_pending(&mut self) -> usize {
        let mut marked = 0;
        for i in 0..self.arena.len() {
            let idx = i as u32;
            if self.arena.node(idx).is_dir() {
                continue;
            }
            let ocr_target = self.arena.extension(idx).as_deref().is_some_and(|ext| {
                crate::indexer::extract::image::is_supported_image_extension(ext)
                    || ext.eq_ignore_ascii_case("pdf")
            });
            if ocr_target {
                self.arena.node_mut(idx).set_content_pending(true);
                marked += 1;
            }
        }
        if marked > 0 {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.mark_ocr_targets_pending()
        {
            eprintln!("[Kensaku] Failed to mark OCR targets pending: {}", e);
        }
        marked
    }

    pub fn mark_images_pending(&mut self) -> usize {
        let mut marked = 0;
        for i in 0..self.arena.len() {
            let idx = i as u32;
            let node = self.arena.node(idx);
            if node.is_dir() {
                continue;
            }
            // extension check
            let ext = self.arena.extension(idx);
            let is_img = ext
                .as_deref()
                .is_some_and(crate::indexer::extract::image::is_supported_image_extension);
            if is_img {
                self.arena.node_mut(idx).set_content_pending(true);
                marked += 1;
            }
        }

        if marked > 0 {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }

        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.mark_images_pending()
        {
            eprintln!("[Kensaku] Failed to mark images pending: {}", e);
        }

        marked
    }

    /// Marks files with any of `extensions` for a content re-read. Used when
    /// external extractors are added, removed or changed.
    pub fn mark_extensions_pending(&mut self, extensions: &[String]) -> usize {
        let wanted: std::collections::HashSet<String> = extensions
            .iter()
            .map(|extension| {
                extension
                    .trim()
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|extension| !extension.is_empty())
            .collect();
        if wanted.is_empty() {
            return 0;
        }

        let mut marked = 0;
        for i in 0..self.arena.len() {
            let idx = i as u32;
            if self.arena.node(idx).is_dir() {
                continue;
            }
            let matches = self
                .arena
                .extension(idx)
                .is_some_and(|extension| wanted.contains(&extension.to_ascii_lowercase()));
            if matches {
                self.arena.node_mut(idx).set_content_pending(true);
                marked += 1;
            }
        }
        if marked > 0 {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }

        if let Some(db) = self.db.as_ref() {
            let extensions: Vec<String> = wanted.into_iter().collect();
            if let Err(e) = db.mark_extensions_pending(&extensions) {
                eprintln!("[Kensaku] Failed to mark extensions pending: {}", e);
            }
        }

        marked
    }

    /// Forces a content re-read for every file (used after a content-index
    /// schema migration). Directories keep their metadata.
    pub fn mark_all_content_pending(&mut self) -> usize {
        let mut marked = 0;
        for i in 0..self.arena.len() {
            let idx = i as u32;
            if !self.arena.node(idx).is_dir() {
                self.arena.node_mut(idx).set_content_pending(true);
                marked += 1;
            }
        }
        if marked > 0 {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.mark_all_content_pending()
        {
            eprintln!("[Kensaku] Failed to mark content pending: {}", e);
        }
        marked
    }

    /// Flags one file for a content re-read on the next crawl.
    pub fn set_content_pending(&mut self, path: &Path) {
        if let Some(idx) = self.find_path_index(path) {
            let node = self.arena.node(idx);
            if !node.content_pending() {
                self.arena.node_mut(idx).set_content_pending(true);
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
        }

        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.set_content_pending(path)
        {
            eprintln!(
                "[Kensaku] Failed to mark '{}' pending: {}",
                path.display(),
                e
            );
        }
    }

    pub fn clear_content_pending(&mut self, path: &Path) {
        if let Some(idx) = self.find_path_index(path) {
            let node = self.arena.node(idx);
            if node.content_pending() {
                self.arena.node_mut(idx).set_content_pending(false);
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
        }

        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.clear_content_pending(path)
        {
            eprintln!(
                "[Kensaku] Failed to clear pending content for '{}': {}",
                path.display(),
                e
            );
        }
    }

    pub fn remove(&mut self, path: &Path) {
        if let Some(idx) = self.find_path_index(path) {
            let was_dir = self.arena.node(idx).is_dir();
            let removed_id = self.arena.node(idx).node_id;
            if removed_id > 0
                && let Some(index) = self.id_index.write().as_mut()
            {
                index.remove(removed_id);
            }
            if let Some(bursts) = self.bursts.write().as_mut()
                && (idx as usize) < bursts.len()
            {
                bursts[idx as usize] = 0;
            }

            self.path_index.remove(&self.arena, path);

            let last_idx = self.arena.len().saturating_sub(1) as u32;
            if idx < last_idx {
                let last_path = self.arena.path(last_idx);
                self.path_index.remove(&self.arena, &last_path);

                let last_node = self.arena.nodes[last_idx as usize];
                self.arena.nodes[idx as usize] = last_node;

                let swapped_path = self.arena.path(idx);
                self.path_index.insert(&self.arena, &swapped_path, idx);

                let swapped_id = self.arena.node(idx).node_id;
                if swapped_id > 0
                    && let Some(index) = self.id_index.write().as_mut()
                {
                    index.insert(swapped_id, idx);
                }

                if let Some(bursts) = self.bursts.write().as_mut()
                    && (idx as usize) < bursts.len()
                    && (last_idx as usize) < bursts.len()
                {
                    bursts[idx as usize] = bursts[last_idx as usize];
                }
            }
            if idx < last_idx {
                for node in self.arena.nodes.iter_mut() {
                    if node.parent == last_idx {
                        node.parent = idx;
                    }
                }
            }
            self.arena.nodes.pop();
            if let Some(bursts) = self.bursts.write().as_mut()
                && !bursts.is_empty()
            {
                bursts.pop();
            }

            self.generation.fetch_add(1, Ordering::SeqCst);
            if was_dir {
                self.dir_generation.fetch_add(1, Ordering::SeqCst);
                *self.cached_trie.write() = None;
            }
        }

        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.remove(path)
        {
            eprintln!(
                "[Kensaku] Failed to delete metadata entry '{}': {}",
                path.display(),
                e
            );
        }
    }

    pub fn rename_prefix(&mut self, old: &Path, new: &Path) -> Vec<PathBuf> {
        if old.as_os_str().is_empty() || old == new {
            return Vec::new();
        }

        // conflicts
        let mut conflicts: Vec<PathBuf> = Vec::new();
        for i in 0..self.arena.len() {
            let idx = i as u32;
            let p = self.arena.path(idx);
            if !p.starts_with(old) && p.starts_with(new) {
                conflicts.push(p);
            }
        }
        for path in conflicts {
            self.remove(&path);
        }

        let mut old_paths = Vec::new();
        // Reparent the renamed root (if present) under the new location's ancestors.
        for i in 0..self.arena.len() {
            let idx = i as u32;
            let p = self.arena.path(idx);
            if p == old {
                let new_parent = self.parent_index_for(new);
                if new_parent == NO_PARENT {
                    let full = new.to_string_lossy().into_owned();
                    let (off, len) = self.arena.names.push(&full);
                    let node = self.arena.node_mut(idx);
                    node.parent = new_parent;
                    node.name_off = off;
                    node.name_len = len;
                } else {
                    let node = self.arena.node_mut(idx);
                    node.parent = new_parent;
                }
            }
        }
        // Snapshot the subtree before changing any name.
        //
        // `arena.path()` is reconstructed from the parent chain, so rewriting
        // an ancestor's name changed every descendant's derived path. The
        // single-pass version below that mutated as it scanned therefore
        // matched the renamed root and then failed `strip_prefix(old)` for
        // every child: only the root reached `old_paths`, it has no document
        // of its own, and every `old/**` document stayed in the content index
        // forever under a path that no longer exists.
        let mut moved: Vec<(u32, PathBuf, PathBuf)> = Vec::new();
        for i in 0..self.arena.len() {
            let idx = i as u32;
            let p = self.arena.path(idx);
            let Ok(suffix) = p.strip_prefix(old) else {
                continue;
            };
            let new_path = if suffix.as_os_str().is_empty() {
                new.to_path_buf()
            } else {
                new.join(suffix)
            };
            moved.push((idx, p, new_path));
        }

        for (idx, old_path, new_path) in moved {
            old_paths.push(old_path);
            // update name (file_name) and char_bloom
            let new_name = new_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (name_off, name_len) = self.arena.names.push(&new_name);
            let node = self.arena.node_mut(idx);
            node.name_off = name_off;
            node.name_len = name_len;
            node.char_bloom = crate::search::fuzzy::compute_char_mask(&new_name);
            // store new path? We don't store full path; path derived from parent chain.
            // Parent chain unchanged (directory renamed would require updating parents).
            // For correctness, update parent pointers? Complex. Instead, rebuild path_index after.
        }

        if old_paths.is_empty() {
            return old_paths;
        }

        // Rebuild path_index
        self.path_index.clear();
        for i in 0..self.arena.len() {
            let idx = i as u32;
            let p = self.arena.path(idx);
            // Wait: path() derives from parent chain + names. If we only updated leaf names,
            // parent directories names not updated for rename_prefix of a directory.
            // This is a known limitation; rename_prefix in original updated path strings.
            // To handle directory renames properly, we'd need update all descendants' parent names?
            // Our arena stores names per node; renaming a directory means renaming that node's name,
            // and descendants keep parent pointers, so path() will reflect new dir name. Good.
            // But we updated only nodes whose path starts with old? We updated names for those nodes
            // based on new_path.file_name(), which for a child keeps its own name, not parent. Oops.
            // Actually for a directory rename, the directory node's name should change to new's last component;
            // descendants keep their names. Our loop changed EVERY descendant's name to its new_path last component,
            // which is wrong (child name becomes same as? new_path for child is new/child, file_name=child, okay).
            // For the renamed root itself, new_path=new, file_name=new last comp. Good.
            // So names are okay.
            self.insert_path_index(&p, idx);
        }

        self.generation.fetch_add(1, Ordering::SeqCst);
        self.dir_generation.fetch_add(1, Ordering::SeqCst);
        *self.cached_trie.write() = None;

        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.rename_prefix(old, new)
        {
            eprintln!(
                "[Kensaku] Failed to persist directory rename '{}' -> '{}': {}",
                old.display(),
                new.display(),
                e
            );
        }

        old_paths
    }

    pub fn record_open(&self, path: &Path) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        {
            let mut usage = self.usage.write();
            let entry = usage.entry(path.to_path_buf()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = now;
        }

        if let Some(db) = self.db.as_ref()
            && let Err(e) = db.record_open(path)
        {
            eprintln!("[Kensaku] Failed to persist open in database: {}", e);
        }
    }

    pub fn get_usage(&self, path: &Path) -> (u32, u64) {
        self.usage.read().get(path).copied().unwrap_or((0, 0))
    }

    /// The materialized entry list for the current generation.
    ///
    /// Returned behind an `Arc` of immutable rows, so a query can hold the list
    /// while another thread mutates the store: the worst case is one query
    /// seeing a list one generation old, which is what it read before the cache
    /// existed.
    pub fn entries_snapshot(&self) -> Arc<Vec<crate::indexer::walker::FileEntry>> {
        let current_gen = self.generation.load(Ordering::SeqCst);
        // `parking_lot` locks are not poisoned, so there is no Result to unwrap
        // here: the same shape as `cached_trie` above.
        {
            let cache = self.cached_entries.read();
            if let Some((cached_gen, entries)) = cache.as_ref()
                && *cached_gen == current_gen
            {
                return Arc::clone(entries);
            }
        }

        let built = Arc::new(
            (0..self.arena.len())
                .map(|index| self.materialize(index as u32))
                .collect::<Vec<_>>(),
        );
        *self.cached_entries.write() = Some((current_gen, Arc::clone(&built)));
        built
    }

    pub fn get_directory_trie(&self) -> Arc<DirectoryTrie> {
        let current_gen = self.dir_generation.load(Ordering::SeqCst);
        {
            let cache = self.cached_trie.read();
            if let Some((cached_gen, ref trie)) = *cache
                && cached_gen == current_gen
            {
                return Arc::clone(trie);
            }
        }

        // materialize
        let mut entries = Vec::with_capacity(self.arena.len());
        let mut path_index = HashMap::with_capacity(self.arena.len());
        for i in 0..self.arena.len() {
            let idx = i as u32;
            let e = self.materialize(idx);
            path_index.insert(e.path.clone(), i);
            entries.push(e);
        }

        let new_trie = Arc::new(DirectoryTrie::build(&entries, &path_index, current_gen));
        let mut cache = self.cached_trie.write();
        *cache = Some((current_gen, Arc::clone(&new_trie)));
        new_trie
    }

    pub fn get(&self, path: &Path) -> Option<FileEntry> {
        self.find_path_index(path).map(|idx| self.materialize(idx))
    }

    pub fn get_index(&self, path: &Path) -> Option<u32> {
        self.find_path_index(path)
    }

    pub fn materialize(&self, idx: u32) -> FileEntry {
        let node = self.arena.node(idx);
        let extension =
            crate::indexer::walker::intern_extension(self.arena.extension(idx).as_deref());
        let path = self.arena.path(idx);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.arena.name(idx).to_string());
        FileEntry {
            id: node.node_id,
            path,
            name,
            extension,
            size: node.size,
            modified: node.mtime,
            is_dir: node.is_dir(),
            dev: node.dev,
            ino: node.ino,
            char_bloom: node.char_bloom,
            content_pending: node.content_pending(),
        }
    }

    pub fn get_by_node_id(&self, node_id: u64) -> Option<FileEntry> {
        let idx = {
            let mut guard = self.id_index.write();
            if guard.is_none() {
                let mut index = IdIndex::new();
                for i in 0..self.arena.len() {
                    let id = self.arena.node(i as u32).node_id;
                    if id > 0 {
                        index.insert(id, i as u32);
                    }
                }
                *guard = Some(index);
            }
            guard.as_ref()?.get(node_id)?
        };
        Some(self.materialize(idx))
    }

    pub fn get_index_by_node_id(&self, node_id: u64) -> Option<u32> {
        let mut guard = self.id_index.write();
        if guard.is_none() {
            let mut index = IdIndex::new();
            for i in 0..self.arena.len() {
                let id = self.arena.node(i as u32).node_id;
                if id > 0 {
                    index.insert(id, i as u32);
                }
            }
            *guard = Some(index);
        }
        guard.as_ref()?.get(node_id)
    }

    pub fn path_index_map(&self) -> HashMap<PathBuf, usize> {
        let mut map = HashMap::with_capacity(self.arena.len());
        for i in 0..self.arena.len() {
            let idx = i as u32;
            map.insert(self.arena.path(idx), i);
        }
        map
    }

    /// Assigned burst for a node, if it is an image that has been grouped.
    pub fn burst_for_node_id(&self, node_id: u64) -> Option<u64> {
        let idx = self.get_index_by_node_id(node_id)?;
        let bursts = self.bursts.read();
        let bursts = bursts.as_ref()?;
        if idx as usize >= bursts.len() {
            return None;
        }
        let burst = bursts[idx as usize];
        if burst == 0 { None } else { Some(burst) }
    }

    /// Assigned burst for a path. The hash lookup is the same one search
    /// already uses for metadata, so collapsing results stays O(results).
    pub fn burst_for_path(&self, path: &Path) -> Option<u64> {
        let idx = self.find_path_index(path)?;
        let node_id = self.arena.node(idx).node_id;
        self.burst_for_node_id(node_id)
    }

    /// Drops a stale burst assignment while a changed image waits for a new
    /// perceptual hash/backfill pass.
    pub fn clear_burst_for_path(&mut self, path: &Path) {
        let Some(idx) = self.find_path_index(path) else {
            return;
        };
        if let Some(bursts) = self.bursts.write().as_mut()
            && (idx as usize) < bursts.len()
        {
            bursts[idx as usize] = 0;
        }
    }

    /// Applies burst assignments returned by the database after a merge.
    pub fn apply_bursts(&mut self, updates: &[(PathBuf, u64)]) {
        let mut bursts = self.bursts.write();
        if bursts.is_none() {
            *bursts = Some(vec![0; self.arena.len()]);
        }
        let bursts = bursts.as_mut().expect("bursts allocated");
        for (path, burst) in updates {
            let Some(idx) = self.find_path_index(path) else {
                continue;
            };
            if idx as usize >= bursts.len() {
                bursts.resize(self.arena.len(), 0);
            }
            let node_id = self.arena.node(idx).node_id;
            if node_id > 0 {
                bursts[idx as usize] = *burst;
            }
        }
    }

    pub fn db(&self) -> Option<Arc<MetadataDb>> {
        self.db.as_ref().map(Arc::clone)
    }

    pub fn path_index_len(&self) -> usize {
        self.path_index.len()
    }

    pub fn path_index_capacity(&self) -> usize {
        self.path_index.capacity()
    }

    pub fn id_index_len(&self) -> usize {
        self.id_index.read().as_ref().map(IdIndex::len).unwrap_or(0)
    }

    pub fn bursts_allocated(&self) -> usize {
        self.bursts
            .read()
            .as_ref()
            .map(|bursts| bursts.len())
            .unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.arena.len()
    }

    pub fn is_empty(&self) -> bool {
        self.arena.is_empty()
    }

    // Accessors for search hot path
    pub fn node(&self, idx: u32) -> &Node {
        self.arena.node(idx)
    }
    pub fn name(&self, idx: u32) -> &str {
        self.arena.name(idx)
    }
    pub fn path(&self, idx: u32) -> PathBuf {
        self.arena.path(idx)
    }
    pub fn extension(&self, idx: u32) -> Option<String> {
        self.arena.extension(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempStore {
        store: Option<MetadataStore>,
        dir: PathBuf,
    }

    impl TempStore {
        fn new(tag: &str) -> Self {
            let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "kensaku-metadata-store-test-{}-{}-{}",
                tag,
                std::process::id(),
                seq
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            let db = MetadataDb::open_in_dir(&dir).expect("open temp db");
            Self {
                store: Some(MetadataStore::with_db(db)),
                dir,
            }
        }

        fn store(&mut self) -> &mut MetadataStore {
            self.store.as_mut().expect("store open")
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            self.store.take();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn entry(path: &Path, is_dir: bool) -> FileEntry {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        FileEntry {
            id: 0,
            path: path.to_path_buf(),
            name,
            extension: if is_dir {
                None
            } else {
                crate::indexer::walker::intern_extension(
                    path.extension().and_then(|ext| ext.to_str()),
                )
            },
            size: 1,
            modified: 1,
            is_dir,
            dev: 0,
            ino: 0,
            char_bloom: 0,
            content_pending: false,
        }
    }

    #[test]
    fn mark_all_content_pending_skips_directories() {
        let mut temp = TempStore::new("mark-all");
        let image = temp.dir.join("photo.jpg");
        let doc = temp.dir.join("notes.txt");
        let dir_path = temp.dir.join("folder");

        let store = temp.store();
        store.add(entry(&image, false));
        store.add(entry(&doc, false));
        store.add(entry(&dir_path, true));

        assert_eq!(store.mark_all_content_pending(), 2);
        assert!(store.get(&image).expect("image").content_pending);
        assert!(store.get(&doc).expect("doc").content_pending);
        assert!(!store.get(&dir_path).expect("dir").content_pending);
    }

    #[test]
    fn mark_images_pending_updates_memory_and_only_images() {
        let mut temp = TempStore::new("mark");
        let image = temp.dir.join("photo.jpg");
        let doc = temp.dir.join("notes.txt");
        let dir_path = temp.dir.join("folder.png");

        let store = temp.store();
        store.add(entry(&image, false));
        store.add(entry(&doc, false));
        store.add(entry(&dir_path, true));

        assert_eq!(store.mark_images_pending(), 1);
        assert!(store.get(&image).expect("image").content_pending);
        assert!(!store.get(&doc).expect("doc").content_pending);
        assert!(!store.get(&dir_path).expect("dir").content_pending);

        store.clear_content_pending(&image);
        assert!(!store.get(&image).expect("image").content_pending);

        let db = store.db().expect("database");
        let loaded = db.load_all().expect("load nodes");
        assert!(
            !loaded
                .iter()
                .find(|entry| entry.path == image)
                .expect("image row")
                .content_pending,
            "clear must be persisted"
        );
    }

    #[test]
    fn add_preserves_pending_when_replacing_an_entry() {
        let mut temp = TempStore::new("preserve");
        let image = temp.dir.join("photo.png");

        let mut first = entry(&image, false);
        first.content_pending = true;
        first.size = 10;
        temp.store().add(first);

        let mut replacement = entry(&image, false);
        replacement.size = 20;
        temp.store().add(replacement);

        let stored = temp.store().get(&image).expect("image entry");
        assert!(
            stored.content_pending,
            "replacing an entry must preserve its pending flag"
        );
        assert_eq!(stored.size, 20, "the new metadata must still be applied");
    }

    #[test]
    fn reloading_from_the_database_finds_every_path() {
        let mut temp = TempStore::new("reload");
        let root = temp.dir.join("root");
        let store = temp.store();
        store.add(entry(&root, true));
        // Same file names in every directory, and enough rows to force the
        // path index to grow several times while loading.
        let names = ["index.js", "README.md", "notes.txt", "a.png"];
        let mut paths = vec![root.clone()];
        for d in 0..30 {
            let dir = root.join(format!("dir{d}"));
            store.add(entry(&dir, true));
            paths.push(dir.clone());
            for f in 0..20 {
                let file = dir.join(format!("{f}-{}", names[f % names.len()]));
                let same_name = dir.join(names[f % names.len()]);
                store.add(entry(&file, false));
                store.add(entry(&same_name, false));
                paths.push(file);
                paths.push(same_name);
            }
        }
        paths.sort();
        paths.dedup();

        temp.store.take();
        let db = MetadataDb::open_in_dir(&temp.dir).expect("reopen db");
        let reloaded = MetadataStore::with_db(db);

        for path in &paths {
            let idx = reloaded.get_index(path);
            assert!(idx.is_some(), "lost after reload: {}", path.display());
            assert_eq!(&reloaded.path(idx.unwrap()), path);
        }
        assert_eq!(reloaded.len(), paths.len());
    }
}
