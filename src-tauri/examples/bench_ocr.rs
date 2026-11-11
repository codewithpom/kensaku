//! Real-model OCR throughput through the bounded extraction pipeline.
//! Set KENSAKU_OCR_PACK_DIR and ORT_DYLIB_PATH, then pass a directory of images.
#[cfg(feature = "accurate-ocr")]
fn main() {
    use kensaku_lib::{indexer::extract, ocr};
    use rayon::prelude::*;
    use std::{path::PathBuf, time::Instant};

    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ocr-ui-text")
        });
    let mut paths: Vec<_> = std::fs::read_dir(&root)
        .expect("image directory")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg")
                })
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no images in {}", root.display());
    ocr::set_engine(ocr::ACCURATE);
    extract::set_ocr_enabled(true);
    let start = Instant::now();
    ocr::self_test().expect("real OCR runtime and pack must be usable");
    println!("Initialization: {:.3} s", start.elapsed().as_secs_f64());
    // Callers wait on extraction slots; all inference still runs in the app pool.
    let callers = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .unwrap();
    let start = Instant::now();
    let results: Vec<_> = callers.install(|| {
        paths
            .par_iter()
            .map(|path| extract::extract_document_checked(path, 100_000))
            .collect()
    });
    let complete = results
        .iter()
        .filter(|(doc, retry)| doc.is_some() && !retry)
        .count();
    let retries = results.iter().filter(|(_, retry)| *retry).count();
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "Images: {}; complete: {complete}; pending: {retries}",
        paths.len()
    );
    println!(
        "Extraction: {elapsed:.3} s; {:.2} images/s",
        paths.len() as f64 / elapsed
    );
    assert_eq!(complete, paths.len(), "every fixture must finish");
}

#[cfg(not(feature = "accurate-ocr"))]
fn main() {
    panic!("bench_ocr requires the accurate-ocr feature");
}
