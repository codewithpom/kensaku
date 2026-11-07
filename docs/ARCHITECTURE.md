# Architecture

> Status: shipped `v1.0.16`. This documents the code in this repository, not a
> future plan. Anything marked **not shipped** is called out explicitly.

Kensaku is a local index with four retrieval tiers and three clients: the desktop
app, the CLI, and an MCP server. There is one Rust library crate (package
`kensaku`, library `kensaku_lib`) plus a small CLI workspace (`cli/`) with
`kensaku-cli`, `kensaku-daemon`, and the optional `kensaku-quick` UI, and a React
frontend. No indexing logic lives in the webview.

## 1. Process model

| Process | Built from | Role |
|---|---|---|
| `kensaku` | `src-tauri/` (Tauri 2) | Desktop app. Hosts the indexer in-process on Windows; on Unix it can start the daemon sidecar. |
| `kensaku-daemon` | `cli/src/bin/kensaku-daemon.rs` | Owns a warm index for CLI/MCP clients. Unix only in practice. |
| `kensaku-cli` | `cli/src/main.rs` | `search`, `state`, `mcp`, `serve`, `connect`. |

**Daemon-first, with a read-only fallback.** On Unix the GUI calls
`DaemonClient::ensure()`, which pings the socket and, if nobody answers, spawns
`kensaku-daemon` from next to the app binary or from `PATH`. CLI and MCP prefer
the daemon too. When it is absent:

- the GUI uses its in-process `IndexerManager` (the only writer on Windows);
- CLI and MCP open the index read-only in-process.

The IPC protocol is newline-delimited JSON-RPC 2.0 over a Unix domain socket or
a namespaced Windows named pipe (`interprocess`). Every request carries the
token from `<config>/daemon.token` and `protocol_version`; the client checks
both before executing. The daemon owns `kensaku.lock`; the GUI owns a separate
`kensaku-gui.lock`, so a second window cannot start even when the daemon is
running. A second launch writes `window.request` and exits; the running app
polls it, which is how `--toggle` and normal shortcut launches work on every OS.

```
┌─────────────┐   ┌─────────────┐   ┌──────────────────┐
│  Tauri GUI  │   │  kensaku-   │   │  MCP clients     │
│  (React)    │   │  cli        │   │  (stdio / HTTP)  │
└──────┬──────┘   └──────┬──────┘   └────────┬─────────┘
       │ in-process      │ JSON-RPC          │ JSON-RPC / MCP
       │ or daemon       │                   │
       ▼                 ▼                   ▼
┌─────────────────────────────────────────────────────────┐
│  kensaku-daemon (Unix)  ·  or in-process IndexerManager  │
│  single writer · token + protocol check · single lock    │
├──────────────┬──────────────────┬───────────────────────┤
│ metadata     │ content          │ semantic              │
│ SQLite WAL   │ Tantivy BM25     │ i8 static table       │
│ + arena      │ + folded field   │ (+ optional smart)    │
├──────────────┴──────────────────┴───────────────────────┤
│ walker · extractors · watcher · embed queue · updater    │
└─────────────────────────────────────────────────────────┘
```

Windows note: `DaemonClient::ensure()` returns `false` on non-Unix targets, so a
Windows install runs everything in the GUI process. The CLI still works and
opens the index read-only.

## 2. On-disk layout

Everything lives under the OS config directory:

| OS | Path |
|---|---|
| Linux | `~/.config/kensaku/` |
| macOS | `~/Library/Application Support/kensaku/` |
| Windows | `%APPDATA%\kensaku\` |

`KENSAKU_DATA_DIR` overrides the location. A `portable` or `kensaku.portable`
marker next to the executable switches everything to `<exe_dir>/data`, which is
what the Windows portable zip uses.

| Path | Contents |
|---|---|
| `config.json` | Settings (see [CONFIG.md](CONFIG.md)). |
| `metadata.db` | SQLite in WAL mode: roots, nodes, semantic units, usage, meta. |
| `index/` | Tantivy content index, schema v4. Old schemas are kept as `index.legacy-*` and rebuilt once. |
| `models/` | Static embedding table and the optional smart model. |
| `ocr/`, `ocr-indra/`, `tessdata/` | Downloaded OCR packs and Tesseract language data. |
| `thumbs/` | 320 px JPEG thumbnails used by the grid and dHash. |
| `kensaku.log`, `crash.log` | Startup milestones and failure-only crash log. |
| `daemon.token`, `kensaku.lock`, `kensaku-gui.lock` | IPC/auth and single-instance state. |

The index is sensitive: it contains file names and extracted text. See
[PRIVACY.md](PRIVACY.md).

## 3. Metadata and the in-memory arena

SQLite is durable truth. Startup builds a compact in-memory arena for the hot
path instead of materializing a `PathBuf` per entry:

- a 64-byte `Node` per file/directory;
- concatenated folded names plus a path index;
- a 64-bit character bloom per name used as a cheap prefilter;
- a small delta buffer for watcher mutations, merged at idle.

Identity is `(device, inode)` / Windows `file_id`, never the path, so a rename
updates one row and never re-extracts content. Unavailable roots (unplugged
drives, unmounted shares) are preserved rather than pruned.

`FileEntry` is the materialized query-time view. Its exact size is printed by
the `bench` harness; the reference footprint is ~128 bytes per entry, which is
the number used in [BENCHMARKS.md](BENCHMARKS.md).

## 4. Index tiers

Four tiers run or short-circuit in this order. Exact work always gets the first
chance to fill the result limit.

### 4.1 Fuzzy filename (`search/fuzzy.rs`)

`nucleo-matcher` over the arena, with path/name weighting, the character-bloom
prefilter, and top-k selection. This is the tier measured by the `bench`
example and the one that answers most keystrokes.

### 4.2 BM25 content (`indexer/content.rs`, Tantivy)

Schema v4 stores:

| Field | Type | Purpose |
|---|---|---|
| `path` | string, stored | delete-by-path, join to metadata, result identity |
| `filename` | text, stored | filename relevance |
| `content` | text, not stored | BM25 |
| `content_folded` | text, trigram, freqs only | look-alike and substring tier |
| `content_raw` | stored, not indexed | verify folded candidates |
| `modified` | u64, stored + fast | `modified:` |
| `ext` | string, indexed | pushdown for `ext:` |
| `size` | u64, fast | pushdown for `size:` |

Schema v4 dropped trigram positions because folded search only issues boolean
term queries and verifies hits against `content_raw`; positions were the largest
single per-file index cost. `lang:` is handled in the metadata layer rather than
stored as a Tantivy fast field.

The custom `kensaku` tokenizer emits interleaved unigrams and bigrams for Han,
Hiragana, Katakana, Bopomofo, and Hangul, with exact offsets and sequential
positions. Latin, code, and paths keep simple tokenizer behavior. Details:
[LANGUAGES.md](LANGUAGES.md).

Positive `ext:` and `size:` filters become `Must` clauses before top-k, so
BM25 never scores documents the metadata filter would reject. Other filters
remain post-filters; the metadata check is authoritative.

### 4.3 Semantic (static vectors, `embed/`)

The default model is `potion-base-8M`-style static embeddings: tokenize →
table lookup → weighted mean → normalize. The shipped table is i8 with per-row
scales (~7.3 MiB); `semantic_f32_table` keeps the f32 form (~30 MB) for
comparison or rollback. No ONNX graph and no GPU at query time.

Units are 240-token windows with 48 overlap, up to `content_max_semantic_units`
(48) per document. Files under the window size get one doc-level unit. Vectors
are deduplicated by content hash, and `model_id` is stored with every unit so a
model change never mixes vector spaces. Exact scan is used at desktop scale; an
ANN index is deliberately not built below hundreds of thousands of units.

An optional `smart` model compiles behind the `smart-embeddings` feature. It is
opt-in, uses ONNX Runtime, and falls back to the static table with a visible
reason if missing.

### 4.4 Folded / substring fallback

When exact tiers leave a shortfall, look-alike normalization (`0/o`, `1/l/i`,
`5/s`, `8/b`, `m/rn`, `w/vv`, `d/cl`) plus Tantivy's trigram tokenizer finds
partial words and OCR misreads. Each candidate is verified against
`content_raw`; a misread is allowed only within the word-length rule. Folded
hits always rank after exact hits and are marked `ContentFolded`.

## 5. Query pipeline

```
input → lexer → AST → router → planner → tiers → RRF → rank → snippet → IPC
```

- **Grammar** (`query/parser.rs`): `type:`, `ext:`, `in:`, `path:`, `modified:`,
  `size:`, `is:`, `lang:`, `name:`, `content:`, `re:`, `mode:exhaustive`,
  quoted phrases, `near[n]`, and `-negation`. Unknown `key:` is literal text;
  parse errors produce a hint, never zero results.
- **Intent** (`query/intent.rs`): deterministic, local plain-language rules turn
  "screenshots from last week" or "biggest files" into filters and sort. Every
  inference is a removable chip. No model is involved. Explicit grammar wins.
- **Scope** (`query/scope_trie.rs`): `in:` resolves through a folded directory
  trie (exact > prefix > fuzzy) and searches a cached DFS range.
- **Fusion**: reciprocal rank fusion over the tiers, then exact/prefix name
  bonuses, smooth recency, frecency from opens, and a bounded depth penalty.
  Ties break on path, so results are deterministic.
- **Exhaustive mode** skips ranked tiers and enumerates by path, capped at 10k.
  Agent cursors continue past the cap with keyset pagination (`k2-` cursors);
  offset cursors (`k1-`) cover ranked pages. `cursor_limit_reached` is explicit
  rather than a silent truncation.

Filters never replace the text; they narrow it. Category counts (facets) are
computed server-side over the full matched set, not the visible page.

## 6. Indexing and extraction

The walker uses `jwalk`, skips hidden entries by default, applies the configured
exclusions, and honors `.gitignore` / `.cursorignore` unless `include_ignored`
is set. Sensitive paths and credential extensions are default-denied. Cloud
placeholders are indexed by name only, so previews do not hydrate them.

Extraction is one pass feeding both the content index and the semantic index:

| Input | Method |
|---|---|
| Text / code / config | Encoding detection (UTF-16 BOM, Latin-1 fallback) → UTF-8. |
| CSV / TSV | Delimiter sniff → rows as `column = "value"`, header repeated. |
| DOCX / PPTX / XLSX / EPUB / ODT / ODS / ODP | ZIP + `quick-xml`; DTDs disabled. |
| PDF | `lopdf` text layer; scanned pages OCR'd one page at a time. |
| Images | PP-OCRv6 by default; optional Indra; Tesseract fallback. |
| Archives | Metadata only. |
| External | Opt-in config-defined executable; no shell, bounded output/time, process-group kill. Zero plugins by default. |

The extraction pool is bounded (up to 8 workers), each file has a timeout, and
extracted text goes through a bounded channel instead of accumulating in memory.
Content policy defaults are **100 MB** maximum file size, **4 MB** extracted
text, **48** semantic units, and `content_depth = "balanced"`. Files above the
file-size cap are still searchable by name and metadata. Retries are capped at
three and reset when the file or OCR settings change.

The watcher (`notify`) coalesces a 400 ms settle window, re-checks freshly
modified files before trusting them, handles rename/delete while paused, and
schedules a scoped rescan on overflow (with a cooldown so a network drive cannot
restart a full walk forever). Perceptual hashing is a separate one-decode-at-a-
time background pass, not part of the crawl.

## 7. OCR

Three local engines, one selected at a time:

| Engine | Config value | Notes |
|---|---|---|
| PP-OCRv6 tiny | `accurate` (default) | ~6.1 MB pack, bundled ONNX Runtime, in-process. |
| Indra | `indra` | ~27 MB pack; deskew, orientation, multi-script routing. |
| Tesseract | `tesseract` | System binary + language data; fallback where ONNX cannot run (Intel Macs, `--no-default-features`). |

PP-OCRv6 ships in normal builds via the default `accurate-ocr` feature. Installers
bundle ONNX Runtime 1.31 (Windows, Apple silicon, Linux x64). The Tesseract
language list is configurable (`ocr_languages`) and exposed in Settings → Search.
Native platform OCR is **not** implemented.

## 8. Agent surface

Two transports, one read-only contract:

- **MCP over stdio** — `kensaku-cli mcp`, newline-delimited JSON-RPC 2.0. Only
  protocol messages go to stdout.
- **Loopback HTTP/SSE** — `kensaku-cli serve [--port 8765]`: MCP Streamable HTTP
  at `POST /mcp`, `GET /health`, `GET /search?q=`, and an SSE keepalive at
  `GET /events`. Bearer token in `<config>/http.token` (0600), loopback bind and
  per-connection peer check. `reindex` is never enabled on this transport.

Default tools: `health_check`, `search`, `read_snippet`, `index_state`,
`regex_search`, `find_symbol`, and `similar_images` **only when a real visual
encoder exists** (it does not in shipped builds). `reindex` is listed only when
`mcp_allow_reindex = true` and the daemon is running. Search pages are capped at
50, snippets at 100 characters, token budgets are explicit, and result offsets
stop at 10,000 with `cursor_limit_reached`.

`agent_scoped_roots` restricts results and single-path reads. Paths are resolved
before comparison, so `..` and symlink escapes do not pass. Credential files and
excluded extensions are refused even inside a scope. File content is returned as
data, never instructions. See [AGENTS.md](AGENTS.md).

## 9. Desktop app

Tauri 2 + React 19 + TypeScript. The frontend is presentation only: search,
home, preview, settings, setup, duplicates, and Stats for Nerds call Tauri
commands, which forward to the daemon or the in-process indexer.

- **Two window modes.** `app` is a normal decorated window; `launcher` is a tray
  window with a global summon shortcut that hides on Esc/blur. `app_mode` is
  storage; `close_to_background` (default `true`) is separate: closing either
  mode hides the window, destroys the webview, keeps the index warm, and trims
  the allocator. Quit is explicit (tray or turning the switch off).
- **Search surface** stays quiet: search field, category chips, results,
  preview, and a thin indexing line. Sorting, filters, and saved searches are
  in the result controls.
- **Grid or list** (`results_view`, default `grid`), 320 px thumbnails, hover
  details, right-click actions, full keyboard navigation, and `Cmd/Ctrl+Shift+L`
  to toggle the layout.
- **Setup** is a guided flow: folders, exclusions, preferences, optional packs,
  first scan, Done. Downloads happen there; Settings shows status and offers
  **Run setup again**.
- **Languages**: English, Simplified Chinese, and Russian, auto-detected or
  chosen in Settings. Query grammar stays English.
- **Themes**: light and dark. The first frame resolves the OS theme before
  paint; `dark` is the fallback when both storage and `matchMedia` fail.

## 10. Updates

`updater.rs` checks the public GitHub releases API about every 12 hours
(`auto_update`, default `true`). The request carries no user data. When an
update is available, Settings → About shows the release notes, asset size, and
progress; download is bounded, compared with the expected size, and verified
against the release's `SHA256SUMS`. The installer runs detached and the app
shuts down cleanly to avoid a process lock. Portable installs are told to
replace the folder instead of being handed a second installation. Builds remain
unsigned; see [INSTALL.md](INSTALL.md).

## 11. Tests and benchmarks

- **522 Rust test attributes** across `src-tauri/src`, plus frontend `vitest`
  tests. CI runs `cargo test --all-targets`, `cargo clippy -D warnings`,
  `tsc --noEmit`, and `bun run build`.
- **Six benchmark harnesses** in `src-tauri/examples/`: `bench`, `index_bench`,
  `crawl_bench`, `index_format_bench`, `mem_probe`, and `facet_bench`. Method,
  corpus, and honest caveats are in [BENCHMARKS.md](BENCHMARKS.md).

## 12. Deliberate non-goals

- No cloud, accounts, telemetry, sync, or cross-device index.
- No bundled LLM and no in-app chat. Agents bring their own model over MCP.
- No write operations through the agent API except the opt-in `reindex`.
- No visual search in the production search path yet: the Beta tab can download
  the CLIP pack, but `active_encoder()` is `None` in non-test builds, so the
  visual tier is skipped and `similar_images` self-hides.
- No native macOS/Windows OCR.
- No ANN index at desktop scale; exact scan is faster and exact.
- No code signing yet; macOS and Windows warn on first launch.

## 13. Module map

```
src-tauri/src/
  agent/        mcp.rs, http.rs, connect.rs, symbols.rs, mod.rs
  embed/        table, units, queue, provider, smart, visual, model
  indexer/      arena, metadata, metadata_db, content, walker, watcher,
                ignore, burst, tokenizer, extract/
  query/        ast, lexer, parser, router, intent, scope_trie
  search/       fuzzy, mod (fusion, ranking, snippets)
  ocr.rs, ocr_indra/   OCR engines and packs
  config.rs, commands.rs, daemon.rs, doctor.rs, updater.rs,
  main_window.rs, maintenance.rs, preview.rs, cli.rs
src/            React UI (App, components, locales en/zh/ru)
cli/            kensaku-cli, kensaku-daemon, optional kensaku-quick
```

Rule: the webview never owns indexing, extraction, or model state. That boundary
is what lets the CLI and MCP reuse the same index.
