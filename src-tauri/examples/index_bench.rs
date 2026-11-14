//! Extraction/indexing throughput harness.
//!
//! Usage: `cargo run --release --example index_bench -- <dir> [max_files]`
//!
//! Walks `<dir>`, then runs the same extraction path the indexer uses (image
//! metadata + text/OCR) in parallel, reporting wall time and throughput. This
//! isolates the slow part of a crawl without touching the real index.

use kensaku_lib::indexer::content::{
    is_indexable_extension, is_indexable_file_name, read_text_content,
};
use kensaku_lib::indexer::extract::image;
use kensaku_lib::indexer::walker::walk_directories;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let root = PathBuf::from(root);

    let excluded: Vec<String> = vec![
        "node_modules".into(),
        ".git".into(),
        "target".into(),
        "dist".into(),
        "build".into(),
        ".cache".into(),
    ];

    let t_walk = Instant::now();
    let entries = walk_directories(std::slice::from_ref(&root), &excluded, false, true);
    let walked = entries.len();
    let jobs: Vec<_> = entries
        .into_iter()
        .filter(|e| {
            !e.is_dir && (is_indexable_extension(e.extension) || is_indexable_file_name(&e.name))
        })
        .take(limit)
        .collect();
    println!(
        "walked {walked} entries in {:?}; extracting {}",
        t_walk.elapsed(),
        jobs.len()
    );

    let skip_image_meta = std::env::var_os("SKIP_IMAGE_META").is_some();
    let done = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let t = Instant::now();
    let hits: usize = jobs
        .par_iter()
        .map(|entry| {
            let path = &entry.path;
            let is_image = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(image::is_supported_image_extension);
            if is_image && !skip_image_meta {
                let _ = image::dimensions(path);
                let _ = image::phash64(path);
            }
            let text = read_text_content(path, 1_048_576);
            if let Some(text) = text {
                bytes.fetch_add(text.len(), Ordering::Relaxed);
            } else {
                return 0;
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(50) {
                println!("  {n} files, {:.1}s", t.elapsed().as_secs_f64());
            }
            1
        })
        .sum();

    let elapsed = t.elapsed();
    let peak_rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        });
    println!(
        "\n{} files with text in {:.2}s ({:.1} files/s); {} bytes of text; {} threads; peak RSS {}",
        hits,
        elapsed.as_secs_f64(),
        hits as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
        bytes.load(Ordering::Relaxed),
        rayon::current_num_threads(),
        peak_rss
            .map(|kb| format!("{:.0} MB", kb as f64 / 1024.0))
            .unwrap_or_else(|| "n/a".to_string())
    );
}
