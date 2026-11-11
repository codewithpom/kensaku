# Benchmarks

Every number here was measured with a release build unless the row says
otherwise. Nothing is extrapolated without saying so. The harnesses live in
`src-tauri/examples/`.

## Harnesses

| Harness | What it measures |
|---|---|
| `bench` | The per-keystroke hot path: walk, in-memory `FileEntry` list, fuzzy filename queries, metadata footprint. Skips SQLite/Tantivy. |
| `index_bench` | Crawl extraction: walk, image dimensions (optionally perceptual hashing), text/OCR extraction. Does not touch the real index. |
| `crawl_bench` | Fresh and incremental crawl into a throwaway index, with counts, wall time, and peak RSS (Linux). |
| `index_format_bench` | 1M synthetic documents: Tantivy, Tantivy + folded field, and SQLite FTS5 trigram (index time/size, query p50/p95, RSS). |
| `mem_probe` | 1M synthetic metadata entries: RSS, path lookup, and (with `KENSAKU_SEARCH_BENCH=1`) fuzzy vs full search latency. |
| `facet_bench` | `compute_facets` over 100k or 1M matched results. |

## Running

```bash
# Query hot path
cargo run --manifest-path src-tauri/Cargo.toml --example bench --release -- ~/Projects

# Crawl extraction (optional file cap)
cargo run --manifest-path src-tauri/Cargo.toml --example index_bench --release -- ~/Pictures 400

# Fresh + incremental crawl in a throwaway index
cargo run --manifest-path src-tauri/Cargo.toml --example crawl_bench --release -- ~/Pictures

# Index-format comparison (one backend per process)
cargo run --manifest-path src-tauri/Cargo.toml --example index_format_bench --release -- \
  tantivy 1000000 /tmp/kensaku-k12-tantivy

# Metadata memory at 1M entries
cargo run --manifest-path src-tauri/Cargo.toml --example mem_probe --release -- 1000000

# Metadata + query-path shape
KENSAKU_SEARCH_BENCH=1 cargo run --manifest-path src-tauri/Cargo.toml \
  --example mem_probe --release -- 1000000

# Facet counts at 1M matched results
cargo run --manifest-path src-tauri/Cargo.toml --example facet_bench --release -- 1000000
```

Always use `--release`. Debug builds are 10–50× slower. The 100k reference
corpora below are one tree of ~100,000 files.

The harnesses use `walker::walk_directories`, so hidden entries and the default
excluded directories (`node_modules`, `.git`, `target`, `dist`, …) are skipped.

## Warm vs cold

- **Cold**: first run after a reboot or after dropping the page cache
  (`sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'` on Linux). The walk is
  dominated by disk metadata reads.
- **Warm**: subsequent runs with the corpus in the page cache. This is what a
  user sees after Kensaku has indexed once.

`bench` warms each query once, then takes 10 samples per query per process run.
Report warm numbers for search latency and cold numbers for the walk.

## What each `bench` phase measures

1. **Walk** (`walker::walk_directories`): traversal plus `stat` metadata (size,
   mtime, dev/ino) and char-bloom construction. Reported as total milliseconds,
   files walked, and files/second.
2. **Metadata store build**: skipped by design. Entries are used directly so the
   benchmark does not pay for SQLite writes.
3. **Fuzzy search**: eight fixed queries (`main`, `index`, `config`, `search`,
   `mod`, `content`, `metadata`, `watcher`) over every entry, top-50 results.
   Each query prints the average and best of 10 runs; the summary averages the
   queries. This covers the char-bloom prefilter, nucleo scoring, path/name
   weighting, and top-k selection.
4. **Memory estimate**: `size_of::<FileEntry>()` × entry count. This is the
   in-memory metadata footprint only; it excludes the path index, Tantivy, and
   SQLite.

## OCR thread-budget benchmark (#170)

`bench_ocr` measures actual PP-OCR extraction through the app's slot-limited
worker pool. It requires the PP-OCRv6 tiny pack and the same ONNX Runtime used
by `.github/workflows/ocr.yml`. It fails if the engine cannot initialize or any
image remains pending. It does not write an index or change user configuration.

```bash
export KENSAKU_OCR_PACK_DIR=/path/to/ocr-pack
export ORT_DYLIB_PATH=/path/to/libonnxruntime
cargo run --manifest-path src-tauri/Cargo.toml --release --example bench_ocr -- src-tauri/tests/fixtures/ocr-ui-text
```

Compare identical fixture directories and runtime/model versions in separate
processes at the base and PR commits. Report initialization separately from
extraction time. Sample process thread count and CPU time during each process;
thread totals include waiting callers and runtime overhead, not just runnable
OCR work. The standard `bench` harness above remains the walk/search regression
check; it does not exercise OCR.

The extraction budget is `max(1, min(8, available_parallelism - 2))`. Each
PaddleOCR session uses one intra-op and one inter-op thread, so inference runs
on the extraction caller rather than a pool per session. Recognition regions
run serially within a document; documents run concurrently. On machines with
one or two available CPUs, one extraction worker is retained for progress.
Background mode applies to crawler/extraction threads, leaving UI threads at
their existing priority.

Local correctness validation for #170 used Windows 10, four available CPUs,
15.9 GiB RAM, ONNX Runtime 1.31.0 and the PP-OCRv6 tiny pack. All 104 UI-text
fixture lines matched exactly; image and scanned-PDF pipeline tests passed.
Release before/after measurements and the reported Linux corpus/keystroke
latency check remain pending: the cold release build was stopped because it
saturated the development machine. No throughput improvement is claimed from
these correctness tests.

## 100k-file reference: Linux

Measured on Linux (8 vCPU, local SSD), release build, warm page cache. This is
the historical reference set used in earlier releases.

| Phase | Result |
|---|---|
| Walk | ~190 ms per 100k files (~525k files/s) |
| Fuzzy search | ~1.3 ms average per query (best 0.5 ms, worst 2.4 ms) |
| Metadata footprint | 128 B per entry (~12.8 MB per 100k files) |

These remain useful as a regression target on the same class of machine, but
they were collected before the current parallel fuzzy scan and before the
Windows test below. Do not compare them across platforms.

## 100k-file reference: Windows 1.0.16

Fresh run for this release. Synthetic corpus of **100,201 entries** (200 folders
× 500 empty `file-NNNNNN.txt` files) on the Windows system drive.

**Machine:** AMD Ryzen 7 7435HS (8 cores / 16 threads), 25.4 GB RAM, Windows 11,
NTFS, release build, 25.4 GB free. Antivirus real-time scanning was left on.

| Phase | Result |
|---|---|
| Walk | 9.3–10.0 s (~10.0–10.8k files/s) |
| Fuzzy search, default rayon pool (16 threads) | **17.2–18.8 ms average** (best 8.3 ms, worst 20.6 ms) |
| Fuzzy search, 4 rayon threads | 3.1 ms average |
| Fuzzy search, 8 rayon threads | 3.5 ms average |
| Fuzzy search, 1 rayon thread | 6.1 ms average |
| `FileEntry` | 128 bytes |
| Metadata footprint | 12.2 MB per 100,201 entries |
| Arena `Node` | 64 bytes |

**Why Windows is slower here.** Two effects, both real:

1. **The walk is NTFS + antivirus bound.** 100k tiny files cost ~10 s on this
   machine, versus ~190 ms on the warm Linux reference. The walk is not the
   per-keystroke path; it is the first scan.
2. **Rayon oversubscription hurts a cheap prefiltered scan.** The char-bloom
   prefilter rejects most entries without scoring them, so the work per entry is
   tiny. At 16 threads the scheduling and collection overhead dominates; 4–8
   threads are faster on this corpus. This is a known scheduling cost, not a
   correctness issue. `RAYON_NUM_THREADS=4` reproduces the faster number.

Treat the Windows fuzzy numbers as the honest current measurement, not a
guarantee. The same build on the Linux reference machine is in the low
single-digit milliseconds.

## p50 / p95

`bench` prints averages. To get percentiles, repeat the whole run and take
percentiles across runs (each run already aggregates 10 samples):

```bash
for _ in $(seq 1 20); do
  cargo run --manifest-path src-tauri/Cargo.toml --example bench --release -- ~/Projects \
    | awk '/Average across/ {print $(NF-1)}'
done | sort -n | awk '{a[NR]=$1} END {printf "p50 %.3f ms\np95 %.3f ms\n", a[int(NR*0.5)], a[int(NR*0.95)]}'
```

Use the same approach for walk time. Run on an otherwise idle machine, disable
power saving, and keep the corpus on local storage.

## Indexing and extraction

The crawl is a single extraction pass: extracted text feeds both Tantivy and the
semantic index, so an image is OCR'd once rather than once per index. Extraction
concurrency is bounded (up to 8 workers), and the indexer commits through a
bounded channel instead of collecting every document in memory. Perceptual image
hashing is **not** part of the crawl: dimensions come from the header, and
hashes are filled in afterwards by a one-decode-at-a-time background pass used
by `/dupes`.

The rows below are measured on the development machine (Linux, local SSD).
Unless marked release, they are debug-build numbers and therefore conservative;
`make dev` optimizes dependency code, so it is much closer to release than a
plain debug build. OCR time is dominated by the external process and is
build-independent.

| Workload | Result |
|---|---|
| Fresh crawl, 703 mixed files (400 images + 300 text), SQLite + Tantivy + embeddings | ~23–27 s |
| Warm re-crawl of the same tree | ~0.7 s |
| Earlier reference: fresh crawl, 101,101 mixed files, first run | 184 s, peak 183 MB |
| Earlier reference: re-index with nothing changed, same corpus | 0.56 s |
| Indexer process peak RSS, text-only / mixed | ~73 MB / ~73 MB |
| 200 screenshots, OCR | ~12 s (~17 files/s) |
| 400 screenshots, OCR | ~26 s (~15 files/s) |
| PP-OCRv6 pack | 1,780,590 B detector + 4,462,639 B recognizer + 27,156 B dict (~6.1 MB) |
| Static embedding table (i8) | 29,528 × 256: f32 30,236,672 B (~28.8 MiB) → i8 7,559,168 B + 118,112 B scales (~7.3 MiB) |
| 1M synthetic docs, Tantivy baseline | 4.8 s / 66.7 MB / 76 MB peak RSS |
| 1M synthetic docs, Tantivy + folded field | 17.0 s / 99.6 MB / 67 MB peak RSS |
| 1M synthetic docs, SQLite FTS5 trigram | 22.2 s / 918.4 MB / 147 MB peak RSS |
| 1M folded index, 1 / 2 / 4 writer threads | 17.6 s / 69 MB / 99.6 MB; 9.3 s / 116 MB / 107.5 MB; 5.3 s / 201 MB / 110.0 MB |
| 1M metadata arena | 1,001,001 nodes: 91.3 MB RSS delta (96.4 MB total process RSS); path lookup p50 0.271 µs / p95 0.521 µs; 100k filename search 0.305 ms |
| 1M query path (`mem_probe`) | isolated fuzzy p50 3.5–5.2 ms; full `search_response_fast` p50 207 ms (was 675 ms); 100k full path 24 ms (was 73 ms) |
| Facets, release | 100k matched: p50 3.1 ms / p95 3.4 ms; 1M: p50 17.3 ms / p95 18.4 ms |
| Result grouping, release, 50 results | p50 6,252 ns / p95 6,502 ns |
| MCP search over stdio + daemon, 1-file index | p50 37.4 ms / p95 39.8 ms over 60 calls |
| MCP local read-only fallback, 100-file fixture | p50 5.9 ms / p95 6.7 ms |
| CJK index growth | 50 docs × ~100 CJK chars: 4,944 B (SimpleTokenizer) → 48,128 B (CJK + folded), 9.73× |
| Golden query set (43 non-visual queries) | Recall@1 0.9767, Recall@5 1.0000, MRR 0.9884, NDCG@10 0.9914; p50 5.881 ms / p95 6.676 ms (debug) |
| Folded tier, 500 docs (debug) | `invoice` p50 172 µs / p95 262 µs; index growth 1.61× with stored raw text |

The mixed crawl used the fallback embedder; a real install adds the ~7.3 MiB
i8 table when the semantic pack is installed. Desktop RSS is dominated by the
OS webview and is not comparable with the indexer-process numbers above.

### OCR quality note

PP-OCRv6 and Tesseract both scored **104/104 lines exact** on a frozen 104-image
UI-text fixture (~0.11 s/image for Tesseract including process startup). That
fixture does **not** demonstrate a quality gap; the reported PP-OCR advantage on
small sidebar labels from the original bake-off was not reproduced. PP-OCRv6
engine load is ~0.2 s in debug; per-image PP-OCR latency is not measured.

### Not measured or not shipped

- **Visual search / CLIP:** the Beta settings tab can download the pinned pack,
  but no production encoder is wired into search. Similar-image recall and
  latency are unmeasured, and `similar_images` self-hides in shipped builds.
- **Smart embedding pack:** the code path compiles behind
  `--features smart-embeddings`, but real-model recall, download size, and peak
  RSS are not measured. The static table remains the shipped path.
- **Native OCR:** no implementation.
- **1M files:** the walk/search numbers are extrapolated, not measured. See
  below.

## 1M-file extrapolation caveat

The 1M figures are **extrapolated, not measured**: ~1.9 s walk, ~28 ms fuzzy
search, ~128 MB of entry metadata. Linear scaling is optimistic:

- the path `HashMap` and entry vector outgrow CPU caches, so lookups and scoring
  become memory-bound;
- filesystem traversal is less cache-friendly at scale and varies wildly with
  directory shape;
- top-k selection over 1M candidates costs more than the prefilter saving.

Holding a < 15 ms search target at 1M files needs a stronger prefilter
(trigram/prefix index) or a sharded scan. Treat the 1M figures as a planning
budget until a real corpus is measured.

## Idle CPU while hidden

Sample the hidden process with `pidstat` (Linux):

```bash
pidstat -u -p <kensaku-pid> 1 30
```

Or read `/proc/<pid>/stat` (utime+stime) twice a second and divide by the number
of CPUs. Report average %CPU over 30 s while the window is hidden.
