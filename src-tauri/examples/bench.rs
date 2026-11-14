use kensaku_lib::indexer::arena::Node as ArenaNode;
use kensaku_lib::indexer::walker;
use kensaku_lib::search::fuzzy::FuzzySearcher;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let root = PathBuf::from(root);

    println!("=== Kensaku Benchmark ===");
    println!("Root: {}", root.display());

    // Phase 1: Walk
    let t0 = Instant::now();
    let excluded: Vec<String> = vec![
        "node_modules".into(),
        ".git".into(),
        "target".into(),
        "dist".into(),
    ];
    let entries = walker::walk_directories(std::slice::from_ref(&root), &excluded, false, true);
    let walk_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("\n--- Indexing ---");
    println!("Files walked: {}", entries.len());
    println!("Walk time: {:.1} ms", walk_ms);
    println!(
        "Files/sec: {:.0}",
        entries.len() as f64 / (walk_ms / 1000.0)
    );

    // Phase 2: Use entries directly (skip SQLite for benchmark speed)
    println!("Metadata store build: skipped (using entries directly)");

    // Phase 3: Fuzzy search benchmarks
    let queries = [
        "main", "index", "config", "search", "mod", "content", "metadata", "watcher",
    ];
    println!("\n--- Fuzzy Search ({} files) ---", entries.len());

    let all_entries = &entries;
    let candidate_indices: Vec<usize> = (0..all_entries.len()).collect();
    let mut total_ms = 0.0;
    let mut min_ms = f64::MAX;
    let mut max_ms = 0.0f64;

    for q in &queries {
        // Warm up
        let mut fs = FuzzySearcher::new();
        let _ = fs.search(q, all_entries, &candidate_indices, 50);

        // Benchmark (10 runs)
        let mut times = Vec::new();
        for _ in 0..10 {
            let mut fs = FuzzySearcher::new();
            let t = Instant::now();
            let results = fs.search(q, all_entries, &candidate_indices, 50);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            let _ = results.len();
        }
        let avg = times.iter().sum::<f64>() / times.len() as f64;
        let best = times.iter().cloned().fold(f64::MAX, f64::min);
        total_ms += avg;
        min_ms = min_ms.min(best);
        max_ms = max_ms.max(best);
        println!("  {:>12} → avg {:.3} ms (best {:.3} ms)", q, avg, best);
    }

    println!(
        "\n  Average across {} queries: {:.3} ms",
        queries.len(),
        total_ms / queries.len() as f64
    );
    println!("  Best single query: {:.3} ms", min_ms);
    println!("  Worst single query: {:.3} ms", max_ms);

    // Phase 4: Memory estimate
    println!("\n--- Memory Estimate ---");
    println!(
        "  FileEntry size: {} bytes",
        std::mem::size_of::<walker::FileEntry>()
    );
    println!(
        "  Entries: {} x {} bytes = {:.1} MB",
        entries.len(),
        std::mem::size_of::<walker::FileEntry>(),
        entries.len() as f64 * std::mem::size_of::<walker::FileEntry>() as f64 / 1_048_576.0
    );
    println!(
        "  Node (arena) size: {} bytes",
        std::mem::size_of::<ArenaNode>()
    );
}
