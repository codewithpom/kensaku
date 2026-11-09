use crate::embed::model::ModelRegistry;
use crate::embed::units::{Unit, UnitStore, chunk_text};
use crate::indexer::IndexingStatus;
use crate::indexer::content::{is_indexable_extension, is_indexable_file_name, read_text_content};
use crate::indexer::metadata_db::MetadataDb;
use parking_lot::RwLock;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Cached chunk vectors for one content hash.
type CachedChunks = Arc<Vec<(usize, usize, Vec<i8>)>>;

pub struct EmbeddingQueue {
    pub model: std::sync::Arc<ModelRegistry>,
    pub unit_store: Arc<RwLock<UnitStore>>,
    pub is_running: Arc<AtomicBool>,
    pub query_in_flight: Arc<AtomicBool>,
    pub status: Option<Arc<RwLock<IndexingStatus>>>,
    pub app_handle: Option<Arc<RwLock<Option<tauri::AppHandle>>>>,
    worker_handle: Option<JoinHandle<()>>,
}

impl EmbeddingQueue {
    pub fn new(
        model: std::sync::Arc<ModelRegistry>,
        unit_store: Arc<RwLock<UnitStore>>,
        status: Option<Arc<RwLock<IndexingStatus>>>,
        app_handle: Option<Arc<RwLock<Option<tauri::AppHandle>>>>,
    ) -> Self {
        Self {
            model,
            unit_store,
            is_running: Arc::new(AtomicBool::new(false)),
            query_in_flight: Arc::new(AtomicBool::new(false)),
            status,
            app_handle,
            worker_handle: None,
        }
    }

    fn emit_status(
        app_handle: &Option<Arc<RwLock<Option<tauri::AppHandle>>>>,
        status: &Option<Arc<RwLock<IndexingStatus>>>,
    ) {
        if let (Some(app_arc), Some(st_arc)) = (app_handle, status) {
            let st = st_arc.read().clone();
            if let Some(handle) = app_arc.read().as_ref() {
                use tauri::Emitter;
                let _ = handle.emit("indexing-status", &st);
            }
        }
    }

    /// Starts the background embedding worker.
    /// Uses a bounded 2-thread Rayon pool and respects `query_in_flight` preemption.
    pub fn start(&mut self, db: Arc<MetadataDb>) {
        if self.is_running.swap(true, Ordering::SeqCst) {
            return;
        }

        let is_running = Arc::clone(&self.is_running);
        let query_in_flight = Arc::clone(&self.query_in_flight);
        let unit_store = Arc::clone(&self.unit_store);
        let status = self.status.clone();
        let app_handle = self.app_handle.clone();
        let model = self.model.clone();

        let handle = thread::Builder::new()
            .name("kensaku-embed-worker".to_string())
            .spawn(move || {
                // Rayon thread pool bounded to max 2 threads per ARCHITECTURE §5.3 / §10
                let embed_pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(2)
                    .thread_name(|i| format!("kensaku-embed-{}", i))
                    .build()
                    .ok();

                let mut hash_cache: HashMap<String, CachedChunks> = HashMap::new();
                let mut was_embedding = false;

                while is_running.load(Ordering::SeqCst) {
                    // Preempt if query in flight
                    if query_in_flight.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }

                    // TODO: Check battery state (e.g. AC power) and skip if on battery power

                    // Fetch unindexed nodes (indexed_gen = 0), priority: recent first, small files first
                    let pending_nodes = match db.get_nodes_for_embedding(16) {
                        Ok(nodes) => nodes,
                        Err(e) => {
                            eprintln!(
                                "[Kensaku Embedder] Failed to get nodes for embedding: {}",
                                e
                            );
                            thread::sleep(Duration::from_millis(1000));
                            continue;
                        }
                    };

                    if pending_nodes.is_empty() {
                        if was_embedding {
                            was_embedding = false;
                            if let Some(st_arc) = &status {
                                let mut st = st_arc.write();
                                st.is_embedding = false;
                                st.units_indexed = unit_store.read().len();
                                st.semantic_ready = st.units_indexed > 0;
                                let next_phase = if st.is_indexing {
                                    st.phase.clone()
                                } else {
                                    "idle".to_string()
                                };
                                st.phase = next_phase;
                            }
                            Self::emit_status(&app_handle, &status);
                        }
                        // Idle poll interval. Kept short so a crawl that stops
                        // this worker does not sit in `join` for a full second.
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }

                    if !was_embedding {
                        was_embedding = true;
                        if let Some(st_arc) = &status {
                            let mut st = st_arc.write();
                            st.is_embedding = true;
                            st.phase = "embedding".to_string();
                        }
                        Self::emit_status(&app_handle, &status);
                    }

                    // Collect all units across the batch first (read + embed in parallel),
                    // then do a single DB transaction + single UnitStore write.
                    let mut batch_units: Vec<(u64, Unit, Vec<i8>, String)> = Vec::new();
                    let mut batch_node_ids: Vec<u64> = Vec::new();

                    for (node_id, path, size, _modified, _existing_hash) in pending_nodes {
                        if !is_running.load(Ordering::SeqCst) {
                            break;
                        }
                        if query_in_flight.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(50));
                            break;
                        }

                        let filename = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let ext = path.extension().and_then(|e| e.to_str());

                        if !is_indexable_extension(ext) && !is_indexable_file_name(&filename) {
                            let _ = db.update_node_indexed_gen(node_id, 1, None);
                            continue;
                        }

                        // The crawl never reads cloud-only files, so they reach
                        // this queue unembedded. Reading one here downloads it
                        // (or, for a phone over Phone Link, pops "offline"),
                        // so mark it done without touching its contents.
                        if crate::indexer::walker::is_cloud_placeholder_path(&path) {
                            let _ = db.update_node_indexed_gen(node_id, 1, None);
                            continue;
                        }

                        let content = match read_text_content(&path, 1_048_576) {
                            Some(c) => c,
                            None => {
                                let _ = db.update_node_indexed_gen(node_id, 1, None);
                                continue;
                            }
                        };

                        let content_hash = format!("{:x}", fnv_hash(&content));

                        let chunks_with_vectors =
                            if let Some(cached) = hash_cache.get(&content_hash) {
                                Arc::clone(cached)
                            } else {
                                let chunks = chunk_text(&content, size as usize);
                                let model_ref = &model;
                                let computed: Vec<(usize, usize, Vec<i8>)> =
                                    if let Some(ref pool) = embed_pool {
                                        pool.install(|| {
                                            chunks
                                                .into_par_iter()
                                                .map(|(start, end, chunk_str)| {
                                                    let vec = model_ref.embed_to_i8(&chunk_str);
                                                    (start, end, vec)
                                                })
                                                .collect()
                                        })
                                    } else {
                                        chunks
                                            .into_iter()
                                            .map(|(start, end, chunk_str)| {
                                                let vec = model_ref.embed_to_i8(&chunk_str);
                                                (start, end, vec)
                                            })
                                            .collect()
                                    };

                                let computed = Arc::new(computed);
                                if hash_cache.len() < 512 {
                                    hash_cache.insert(content_hash.clone(), Arc::clone(&computed));
                                }
                                computed
                            };

                        batch_node_ids.push(node_id);
                        for (start, end, vec) in chunks_with_vectors.iter() {
                            batch_units.push((
                                node_id,
                                Unit {
                                    id: 0,
                                    node_id,
                                    byte_start: *start,
                                    byte_end: *end,
                                    model_id: model.model_id,
                                },
                                vec.clone(),
                                content_hash.clone(),
                            ));
                        }

                        // Mark node as processed (hash stored for dedup)
                        let _ = db.update_node_indexed_gen(node_id, 1, Some(&content_hash));
                    }

                    // Batch delete + insert in one SQLite transaction (one commit
                    // instead of one per unit). The in-memory store is updated
                    // afterwards so searches are not blocked behind disk writes.
                    // A failed BEGIN means another writer holds a transaction on
                    // this connection; its commit covers these rows.
                    let began = db.begin_transaction().is_ok();
                    let _ = db.delete_units_for_nodes(&batch_node_ids);
                    let mut inserted: Vec<(Unit, Vec<i8>)> = Vec::with_capacity(batch_units.len());
                    for (node_id, mut unit, vec, _hash) in batch_units {
                        if let Ok(inserted_id) = db.insert_unit(
                            node_id,
                            unit.byte_start,
                            unit.byte_end,
                            model.model_id,
                            &vec,
                        ) {
                            unit.id = inserted_id;
                            inserted.push((unit, vec));
                        }
                    }
                    if began {
                        let _ = db.commit_transaction();
                    }

                    let mut unit_store_write = unit_store.write();
                    let replaced: std::collections::HashSet<u64> =
                        batch_node_ids.iter().copied().collect();
                    unit_store_write.remove_units_for_nodes(&replaced);
                    for (unit, vec) in inserted {
                        unit_store_write.add_unit(unit, &vec);
                    }
                    drop(unit_store_write);

                    // Update indexing status progress
                    if let Some(st_arc) = &status {
                        let mut st = st_arc.write();
                        st.units_indexed = unit_store.read().len();
                        st.semantic_ready = st.units_indexed > 0;
                    }
                    Self::emit_status(&app_handle, &status);
                }
            })
            .ok();

        self.worker_handle = handle;
    }

    pub fn stop(&mut self) {
        self.is_running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
    }
}

fn fnv_hash(data: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::fnv_hash;

    /// FNV-1a offset basis is the hash of the empty string.
    #[test]
    fn fnv_hash_empty_string_is_offset_basis() {
        assert_eq!(fnv_hash(""), 0xcbf29ce484222325u64);
    }

    /// The hash is deterministic: two calls with the same input produce the
    /// same output.
    #[test]
    fn fnv_hash_is_deterministic() {
        let h1 = fnv_hash("hello kensaku");
        let h2 = fnv_hash("hello kensaku");
        assert_eq!(h1, h2);
    }

    /// Different strings must (in the overwhelming majority of cases) produce
    /// different hashes.
    #[test]
    fn fnv_hash_differs_for_different_inputs() {
        assert_ne!(fnv_hash("foo"), fnv_hash("bar"));
        assert_ne!(fnv_hash("abc"), fnv_hash("ABC"));
        assert_ne!(fnv_hash("a"), fnv_hash("b"));
    }

    /// Single-byte inputs can be verified manually against the FNV-1a spec.
    /// FNV-1a("a") = (offset ^ 0x61) * prime
    #[test]
    fn fnv_hash_single_byte_matches_spec() {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        // Correct FNV-1a order: h = (h XOR byte) * prime
        let expected = (OFFSET ^ b'a' as u64).wrapping_mul(PRIME);
        assert_eq!(fnv_hash("a"), expected);
    }

    /// Hash of "hello" is stable across builds (regression guard).
    #[test]
    fn fnv_hash_hello_is_stable() {
        // Computed independently: echo -n hello | python3 -c
        //   "import sys; h=0xcbf29ce484222325; p=0x100000001b3
        //    [h:=((h^b)*p)&0xFFFFFFFFFFFFFFFF for b in sys.stdin.buffer.read()]
        //    print(hex(h))"
        assert_eq!(fnv_hash("hello"), 0xa430d84680aabd0bu64);
    }
}
