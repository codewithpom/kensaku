# Changelog

All notable changes to Kensaku. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning: [SemVer](https://semver.org/).

## [Unreleased]

### Added
- **Dark-mode tray icon on Windows** (#46). The Windows tray icon now has a
  light, high-contrast variant that is used when the app theme is set to
  `dark`, so the icon stays legible on a dark taskbar. It follows the **app
  theme** (Settings → Appearance), not the Windows taskbar setting, and swaps
  immediately when the theme changes. Both assets (`tray-dark.png` /
  `tray-light.png`) are embedded at compile time; no extra files ship at
  runtime. macOS and Linux are unchanged.

## [1.0.16] — 2026-11-05

### Fixed
- **The launcher hid itself on right-click and on the sort control** (#95). It dismissed on
  any window blur, and two things blur the webview without the user leaving: there was no
  context menu handler, so WebKitGTK drew its own, and the sort control was a native
  `<select>` whose popup is a separate window on Linux. The context menu is suppressed, sort
  is an in-app listbox, and dismissal now ignores focus-stealing popups while still hiding on
  a real click-away.
- **A clean install reported its own startup log as a crash** (#94). Milestones now go to
  `kensaku.log`, and the health check only reports lines that describe a failure.
- **The Windows portable build shipped without OCR** (#94). The zip was assembled by hand and
  omitted `onnxruntime.dll`, so Indra and PP-OCR could never load on a portable install. The
  library now ships in the zip and the build fails if it is missing.
- **Tesseract read every image as English** (#107, #110). `-l` was never passed, whatever
  language data was installed. Languages are now selectable in Settings → Search.
- **Renaming a folder left its documents behind in the index** for ever, because the rename
  path rewrote parent names while deriving child paths.
- **An unreadable folder deleted its whole subtree from the index.** One locked or
  permission-denied directory was indistinguishable from "everything inside was deleted".
- **The watcher ignored `.gitignore` and `.cursorignore`**, so build artifacts entered the
  index on every build and were removed again by the next crawl.
- **A panic during indexing left the index frozen for the rest of the session**: the crawl
  gate was never released, so every later crawl and rebuild returned immediately.
- **Changing the excluded-extension list never removed files already indexed.**
- **Files over 1 MB ignored the "deep content" setting** and always indexed 512 KB from each
  end; the watcher also capped re-indexed files at 100 KB.
- **Reset could close the app and never reopen it**: the replacement process lost the
  single-instance lock race. The lock is released before restarting.
- **Two windows could run at once** when the daemon was present, and `--toggle` started a
  second app instead of summoning the first.
- **Updates had no timeout, no integrity check, and installed truncated downloads.** Every
  request is now bounded, the download is compared with the expected size and verified
  against the release's `SHA256SUMS`, and portable installs are told to update the folder
  instead of being handed a second installation.
- **Escape in Settings silently discarded unsaved edits.**
- **IME input opened files.** The Enter that commits a Chinese or Japanese candidate also
  opened the selected result.
- **Tab trapped keyboard focus** in the result list, so nothing else was reachable.
- **Enter, Space, Cmd+C and Cmd+I acted on the previous query's rows** while a search was in
  flight.
- **MCP could read any file on the machine.** `read_snippet` took an absolute path straight to
  disk, so an empty agent scope (the default) allowed reading `~/.ssh/id_ed25519`. Credential
  files and excluded extensions are now refused.
- **Settings switches had no accessible name** for screen readers.
- **Typing only registered the first character.** The toolbar holding the sort control renders
  only once a query has results, so it mounted on the first keystroke; the focus handler added
  with the in-app sort listbox ran on mount and moved the caret to the sort button. Focus is now
  returned to the trigger only when the list actually closes. Covered by a DOM test that fails
  against 1.0.16.
- **Indexing appeared to run on every launch, forever.** A file whose extraction timed out, or
  whose extraction slot was busy, was marked pending and re-read on the next crawl with no
  limit. A file that always failed was therefore re-decoded on every single launch and the
  crawl never settled. Retries are now capped at 3 and reset when the file changes or OCR
  settings change.

### Changed
- **Search does far less work per keystroke**: filters no longer lowercase every filename and
  path in the corpus, and name sorting no longer allocates inside the comparator.
- **Scanned PDFs are OCR'd one page at a time.** The whole document used to be decoded first,
  around 3 GB for a 20-page scan.
- **Snippets now load for rows scrolled into view**, hover details are fetched on hover
  instead of once per card per search, and stats polling pauses while the window is hidden.
- **Two missing SQLite indexes** made the embedding queue and image burst grouping full-scan
  the nodes table repeatedly.

### Added
- `Cmd/Ctrl+Shift+L` toggles grid and list view (#47).
- OCR language picker in Settings → Search.
- Frontend unit tests (`vitest`), run in CI.
- **Indexing is visible from the settings panel.** A horizontal bar at the top of Settings
  shows the phase (checking folders, indexing, embedding, saving), the file counts and a
  progress bar while a scan runs. Previously a scan started while Settings was open was
  invisible, because the panel covers the status line in the main window.

## [1.0.15] — 2026-11-04

### Fixed
- **Indra OCR crawl stall & lock contention**: Resolved an indexing freeze that could
  stall crawls at 97%+ when processing large batches of images with Indra OCR.
  - Eliminated high-frequency `EmptyWorkingSet` memory trims on Windows that were acquiring
    the kernel virtual memory manager lock after every single extracted file, rate-limiting
    them to at most once every 5 seconds.
  - Replaced the serialized mutex-guarded channel receiver in `SlotPool` with a condition
    variable (`Condvar`) and permit count, preventing caller thread starvation during backpressure.
  - Replaced the blocking lock convoy across parallel Indra OCR engines with non-blocking
    `try_lock()` polling and adaptive thread yields, ensuring idle engines are immediately utilized.
  - Increased single-file OCR extraction timeout budget from 8 s to 45 s for images and scanned
    PDFs, avoiding spurious timeouts and orphaned tasks on dense high-resolution screenshots.
  - Replaced parallel slice operations in skew detection with sequential row processing,
    eliminating nested thread pool overhead on preview thumbnails.
  - Reduced indexing progress update interval from every 25 files to every 5 files, providing
    smooth and responsive UI progress feedback during compute-heavy document processing.

## [1.0.14] — 2026-11-04

### Added
- **Seamless in-app updater**: Updates no longer require manually navigating
  GitHub release pages. When an update is detected, an update badge appears on
  the status bar and in **Settings → About**, showing the release notes, asset
  size, and download progress bar. Users can download and install updates with a
  single click; the installer executes detached and Kensaku cleanly shuts down
  to prevent process lock conflicts.
- **Configurable auto-update checks**: Background update checks (once every
  12 hours) are enabled by default and can be toggled on/off in **Settings → About**.
- **One-click GitHub issue reporting**: Health check failures, OCR warnings,
  and About settings now include a direct "Report Issue" action that copies complete
  system and index diagnostics to the clipboard and opens a pre-filled GitHub
  issue in the default browser.
- **Automated ONNX Runtime build staging**: Development and test builds automatically
  stage the required ONNX Runtime dynamic library (`onnxruntime.dll` / `libonnxruntime.so` /
  `libonnxruntime.dylib`) to target directories via `build.rs`.

### Fixed
- **Indra OCR Cyrillic recall** (#93): Indra OCR now falls back to the comprehensive
  base recognizer (`rec.onnx` + vocabulary) when specialized script models return
  empty or low-confidence results, restoring 100% recall for Cyrillic and mixed-script
  screenshots.
- **ONNX Runtime anti-hijacking**: The dynamic library loader in `ocr.rs` now verifies
  explicit paths (`deps/`, `debug/`, `src-tauri/`, config/model folders) and eliminates
  the bare library fallback on Windows that was silently loading older, incompatible
  copies from `C:\Windows\System32\onnxruntime.dll`.
- **Transparent OCR error reporting**: `self_test()` and text extraction now return
  explicit `OcrOutcome::Unavailable(reason)` and expose detailed diagnostics rather
  than masking failures as silent blank text (`NoText`).
- **Linux AppImage system Tesseract isolation** (#93): Sanitized the execution
  environment when invoking host system `/usr/bin/tesseract` inside AppImages by
  clearing `APPDIR` and restoring `LD_LIBRARY_PATH_ORIG`, preventing crashes caused by
  linking against bundled AppImage libraries.
- **Doctor health auditing**: Added active auditing of `indra_status()` to the Doctor
  suite whenever Indra is configured as the active OCR engine.

## [1.0.13] — 2026-11-04

### Added
- **Desktop app mode** (#89): setup now offers a normal, decorated window in
  the taskbar/Dock (the default for new installs) or the existing tray
  launcher. The mode is switchable later in **Settings → Window**; app mode
  starts visible, is never hidden by the tray item or `--toggle`, and quits on
  close, while launcher mode keeps the global summon shortcut, hides on Esc or
  focus loss, and stays out of the taskbar.
- **Simplified Chinese interface** (#70): the full UI — search, results,
  dialogs, setup, settings, interpretation chips, relative times — is
  translated, follows the system language, and can be changed in Settings.
- Startup diagnostics (#77): milestones and errors are appended to
  `crash.log`, and a window-creation failure now shows a non-blocking native
  dialog with WebView2 guidance on Windows instead of exiting silently.

### Fixed
- **Cyclic reindexing** (#80): overflow events from large or network-backed
  trees no longer restart a full walk on every signal. Rescans skip unchanged
  entries and are coalesced behind a 60-second cooldown, so the indexer stops
  pinning the CPU after the initial crawl.
- Native `<select>` options and groups use theme-aware colors again, making the
  sort and settings dropdowns readable in light and dark mode (#79).

### Changed
- Privacy documentation now distinguishes no telemetry, the roughly 12-hour
  GitHub release check, and model/OCR downloads, instead of claiming that zero
  bytes ever leave the machine (#78).

## [1.0.12] — 2026-11-03

### Added
- `mouse near[5] button`: the `near[x]` proximity operator matches files
  where the terms sit within `x` words of each other. Chains
  (`a near[3] b near[2] c`) and the `near:5` filter form work too; the
  fuzzy, semantic and visual tiers are skipped for such queries because
  word distance is a lexical constraint embeddings cannot honor. Distances
  are safely clamped to 100.
- The data directory is overridable: `KENSAKU_DATA_DIR` (tests, Nix /
  Flatpak-style packaging) has highest precedence, then a `portable` or
  `kensaku.portable` marker file next to the executable keeps everything
  in `<exe_dir>/data` (the Windows portable build is now self-contained),
  before the platform default (`~/.config/kensaku`, `%APPDATA%\kensaku`,
  …). The metadata database no longer hardcodes one location and follows
  the same resolution as every other data path (see `docs/CONFIG.md`).
- Multilingual and format documentation (`docs/LANGUAGES.md`): detailed
  analysis of CJK unigram + bigram segmentation, 50+ programming and data
  formats, and multi-script OCR engine routing.
- Native Windows on ARM builds (#74): the release pipeline also produces
  arm64 assets — `Kensaku_<ver>_arm64_en-US.msi`,
  `kensaku_<ver>_arm64-setup.exe`, `kensaku-portable-<ver>-arm64.zip` and
  `kensaku-cli-windows-aarch64.exe` — staged against Microsoft's
  official `onnxruntime-win-arm64-1.31.0.zip` (SHA-256-pinned), so
  Snapdragon laptops run PP-OCR natively instead of x64 under emulation.
  Added to the v1.0.12 release after review.

### Fixed
- macOS permissions (#73): bundled `Info.plist` with explicit TCC privacy
  usage descriptions (`NSDocumentsFolderUsageDescription`,
  `NSDesktopFolderUsageDescription`, etc.) and ensured child preview
  processes on macOS never receive raw file paths, preventing repeated
  permission prompt loops on macOS Sequoia / Darwin 24.
- Theme first-paint flicker (#72): resolves stored or system theme
  preference before initial paint, eliminating a dark flash on light OS themes.
- Office document extraction: decodes XML entity references (`&amp;`, `&lt;`,
  `&gt;`, `&quot;`, `&apos;`) in DOCX, PPTX, XLSX, EPUB, and ODT files.
- Excel shared strings: accumulates text across all `<t>` tags within a cell
  instead of only keeping the last one.
- PDF single-page extraction: safely caps extraction budget at valid UTF-8
  character boundaries.
- Ignore engine: merges rules when `.gitignore` and `.kensakuignore` or
  `.cursorignore` coexist in the same directory rather than overwriting.
- Windows portable release packaging (#68): automated CI packaging now
  ships `kensaku-portable-<version>-x64.zip` containing the `portable`
  marker file and README.

## [1.0.11] — 2026-10-30

### Added
- Indra OCR (datalab-to/indra-ocr) as a third engine, ported natively to Rust
  on the same ONNX Runtime as PP-OCR: orientation and deskew, script routing
  to four CTC recognizers, RTL reordering and Hangul composition. Text matches
  the Python reference at 97-100% on the sample pages. The ~27 MB model pack
  (v1.1.0, SHA-256-pinned) downloads only when Indra is selected; Indra falls
  back to PP-OCR, then Tesseract, and doctor, Settings and setup report it.
- Setup wizard: launcher-only flow with folder, OCR and shortcut steps.
  Progression blocks until the selected OCR engine is ready, downloads show
  download/verify progress, and an application-type step is gone in favor of
  launcher mode with a close-to-hide note and shortcut capture.

### Changed
- PP-OCR is the default OCR engine again (`indra` was briefly the default;
  existing configs are untouched either way).
- Existing model artifacts are checksum-verified on startup instead of being
  trusted by name; corrupt files are re-downloaded.

### Fixed
- The wizard's OCR pack download no longer triggers a scan before the user
  has picked their folders: a fresh or corrupt config falls back to watching
  the home directory, and the pack-ready callback used to crawl those
  defaults mid-setup (walking `~\AppData`, MTP and network devices).
  `start_indexing` and `start_indexing_when_idle` now refuse to run before
  the setup wizard completes, which also protects the daemon's startup crawl.
- Watcher live updates are covered again on Windows: the config-v6 prune of
  `~\AppData` made every event under `AppData\Local\Temp` (the test
  environment) look excluded, so four watcher tests had been failing since
  1.0.10 on Windows. The tests now watch their temp root explicitly.
- The agent CLI compiles again: since the vendored `paddleocr_rs_onnx`
  (`shared_environment`) landed, the separately-workspaced CLI built the
  plain crates.io crate and failed (`[patch.crates-io]` is per-workspace and
  had to be repeated). The daemon `rescan` RPC now also reloads the config
  the GUI just saved instead of crawling stale in-memory folders.
- Initial indexing shows "Verifying files" instead of a stuck 0%.
- Setup step counts ("03 / 03") after the flow gained a third step.

## [1.0.5] — 2026-10-28

### Fixed
- Theme choice is saved again. The UI renamed its themes to `light` / `dark`
  in 1.0.4, but the backend still only accepted the old `paper` / `void`
  names, so every `set_theme` call failed and the pick never reached disk.
  Both old names are still accepted and existing configs are migrated
  (config v5).
- Red CI on master: `clippy::items_after_test_module` in `memory.rs` (merge artifact) fixed.

## [1.0.4] — 2026-10-28

### Changed
- Redesign: design system, search surface, results, home, onboarding, dialogs, menus, settings.
- Drop glass theme; tighter spacing; rounded-rect search.
- Remove Alt+Space hint from search bar; distinct settings tab icons.
- Master Escape key handling.

## [1.0.3] — 2026-10-27

### Changed
- Scope: Ask-a-file, the local model query refinement, `kensaku-cli ask`, and video previews are removed.
- Quick shortcut: Settings label, Wayland note, and `kensaku --toggle` for desktop keys.
- Keyboard: Cmd/Ctrl+C copies the selected path, Cmd/Ctrl+I opens details.
- Time travel: "last Tuesday" and "N days ago" become time filters.
- README: speed-first hero, measured performance section, new hero image.

## [1.0.2] — 2026-10-27

### Added
- Grid or list results, chosen in the results bar and in Settings.
- Sort dropdown: Best match, Newest, Oldest, Largest, Name.
- Previews for PDFs (first page) and images, shown on cards and in Quick Look (Space).
- Recent files shown as a list on the home screen.
- Save-search and layout controls on the same row as the category filters.
- Tesseract OCR is found inside the app first, before PATH and system installs.

### Changed
- Thumbnails and previews use one 240px size.
- Filters, sort, and layout share one pill style.
- Setup folder step: clearer selection, neutral checkboxes, centered layout.
- Dark-mode borders use the theme color.
- Settings labels: "Meaning search" and "Local model" replace the AI wording.

### Fixed
- Release workflow stages only installable files and checks that artifacts arrived.

## [1.0.1] — 2026-10-27

### Changed
- New UI: neutral light and dark themes with no accent color, the Mac system
  font, and Apple-style motion curves.
- Header is search and Settings only.
- Results are a grid of square cards with infinite scroll. Only what is on
  screen is rendered.
- Hover shows a slim info bar with size, image dimensions, and age.
- Sort is an icon row. Right-click a result for Open, Show in folder, Copy
  path, and Details.
- Onboarding starts with no folders selected.

### Added
- Show thumbnails setting (Settings → Search), on by default.
- Thumbnails are sent to the UI as data URLs, so they render on every platform.

### Fixed
- Infinite scroll no longer repeats the same page request.
- Thumbnails are generated for every visible image, not only the first 30.

## [1.0.0] — 2026-10-27

Kensaku 1.0 is the first release with the full agent surface, optional local
LLM, the search-quality pass, and a cleaned public documentation set. The
desktop app, daemon, CLI, and MCP/HTTP transports are read-only by default and
local-first.

### Added
- Agent surface v2: `find_symbol` (tree-sitter for Rust, Python, JavaScript,
  TypeScript), `.gitignore`/`.cursorignore` parity at index time
  (`include_ignored` opts out), keyset cursors past the 10k exhaustive cap,
  and `agent_scoped_roots` read-only scoping with resolved-path containment.
- Loopback HTTP/SSE transport: `kensaku-cli serve` exposes the MCP tools on
  `127.0.0.1` with a bearer token, JSON or SSE responses, and read-only
  `/health` and `/search` endpoints. `reindex` is never available there.
- One-click agent connect for Claude Desktop, Claude Code, and Cursor via
  Settings → Agents (atomic merge, `.bak` backup), mirrored by
  `kensaku-cli connect <client> [--write]`.
- Optional local LLM (Ollama / LM Studio / OpenAI-compatible; off by default):
  a query compiler that validates filter keys and values before showing chips,
  and `/ask` with path+line citations and no-retrieval-no-answer semantics.
  Non-loopback endpoints require explicit consent and show a cloud indicator in
  the app; the default remains fully local.
- `facet_bench` and a `KENSAKU_SEARCH_BENCH=1` mode for `mem_probe`, plus the
  golden-query integration set in CI.

### Changed
- Search performance: interned extensions (no per-entry `String`), content
  schema v3 with indexed `ext` and fast `size` filter pushdown, allocation-light
  rayon-folded facets (release p50 3.1 ms at 100k, 17.3 ms at 1M), parallel
  metadata materialization, a path index only for trie queries, and parallel
  fuzzy scoring. Full `search_response_fast` p50 is 207 ms at 1M and 24 ms at
  100k; the fuzzy filename tier is 3.6–5.2 ms. Content indexes rebuild once to
  v3 and preserve `index.legacy-*`.
- The Tantivy writer stays single-threaded with a 16 MB arena after measuring
  the 1M trade-off (2 threads were 1.9× faster but used ~47 MB more peak RSS).
- Doctor now checks free disk space alongside its existing per-root
  readability, database, index, watcher, extractor, model, OCR, and version
  checks. The reset marker/restart path has an automated smoke test.
- `RELEASING.md` now documents the draft-release workflow, which uploads every
  bundle, CLI binary, and `SHA256SUMS` to a draft GitHub release on tag push.
- Project version is `1.0.0` in the app, CLI, Tauri config, and docs.

### Fixed
- The ONNX runtime loads dynamically, so older glibc toolchains link again.
  Accurate OCR probes the runtime, contains loader/run panics, reports the
  engine unavailable when the library is missing, and falls back to Tesseract
  instead of losing the file.
- Agent root scoping resolves `..`, `.` and symlinks before comparison;
  out-of-scope reads are rejected.
- One-click connect uses the platform Claude config directory, detects
  project-scoped Claude Code entries, and writes atomically with full-name
  `.bak` backups.
- The query compiler rejects invalid filter values instead of silently
  downgrading them to text terms.
- PP-OCRv6 is compiled into normal builds; Setup/Settings present Tesseract and
  PP-OCRv6 as an either/or choice, and the unsupported native-OCR option is
  disabled rather than faked.
- The fake visual/native paths from the automated pass were removed. Visual
  search remains a Settings → Beta model-download experiment and is not wired
  into search.

### Release notes
- Bundles and the CLI are **unsigned**. See `INSTALL.md` for how to verify a
  download and what the OS warnings mean. Signing/notarization and the
  auto-updater are follow-ups after 1.0.
- Linux: `.deb`, `.rpm`, `.AppImage`; macOS: universal `.dmg`; Windows: `.msi`
  and NSIS `.exe`; plus `kensaku-cli` for each platform and `SHA256SUMS`.

## [0.3.3] — 2026-10-26

### Fixed
- **Images and PDFs were not indexed for existing installs.** Early UI configs
  stored exclusions with a leading dot (`.jpg`, `.pdf`), which the v1 migration
  did not normalize, so those files were skipped forever. A v2 migration strips
  dotted document/image extensions and bumps the config version. Restart after
  upgrading; the startup crawl picks the files up.
- **Thumbnails were blank.** The Tauri asset scope used `$APPCONFIG`, which is
  `<config>/<identifier>`, while thumbs live at `<config>/kensaku/thumbs`. The
  scope now starts with `$CONFIG/kensaku/thumbs/**`.
- **Search took 5–9 s.** Snippets called the full extractor (PDF/OCR) inside the
  query. They now read plain text directly or a cached extraction only; a second
  batched request fills them after the results paint. Measured: 3.86 s → 0.14 s.
- **The metadata DB was 310 MB for 382 rows.** Startup now vacuums when free
  pages exceed 20%, caps the WAL at 64 MB, and checkpoints after crawls
  (310 MB → 604 KB).
- Image previews no longer run OCR; previews are debounced and sequenced.
- `content_docs` reports the real Tantivy document count.

### Changed
- Release pipeline builds bundles and CLI binaries as workflow artifacts;
  publishing is done with `gh` (see `docs/RELEASING.md`).
- Packaging: `bench` is now a cargo example and `kensaku-cli` is a separate
  crate, so Tauri bundles exactly one binary. This fixes the macOS universal
  build failing while trying to copy the extra binaries.

## [0.3.1] — 2026-10-26

### Fixed
- **OCR could not be turned on.** The Settings switch was a disabled placeholder
  and `ocr_enabled` was never read by the backend. The switch now persists to
  config, extraction is gated on it, and changing it re-scans only images.
- **OCR silently produced nothing when tesseract had no language data.** Settings
  now reports engine/language status, can download `eng.traineddata` (~4 MB) into
  Kensaku's own directory, and shows platform install commands when the engine is
  missing. Doctor gained an OCR check.

### Changed
- `content_pending` column lets a settings change re-process only the affected
  files; pending images with no extractable text have stale documents removed.

## [0.3.0] — 2026-10-26

The open-source launch. Seven focused changesets on top of v0.2.0.

### Added
- **Agent surface.** `kensaku-cli` with `search`, `state`, and `mcp`; an MCP stdio
  server exposing `search`, `read_snippet`, and `index_state` over the same
  read-only index. Token-budgeted snippets, ranked and exhaustive modes, path/ext/
  lookback filters. See `docs/AGENTS.md`.
- **Real static embeddings.** Manual safetensors (F32/F16) + BERT WordPiece loader
  for potion-base-8M, loaded once via a global registry; automatic re-embed when
  the model id changes. Hashing estimator remains the fallback.
- **Image support.** Lazy cached thumbnails (320 px JPEG), dimensions, 64-bit dHash,
  perceptual near-duplicate groups merged into `find_duplicates`, thumbnail-forward
  results and preview.
- **Query semantics.** `re:` regex execution, true `name:`/`content:` field scoping
  in BM25, `mode:exhaustive`, and a semantic candidate prefilter.
- **Robustness.** Single-instance lock, atomic config writes with corrupt-file
  backup, unavailable-root preservation, 8-second extraction timeout on a bounded
  pool, directory-rename subtree rewrite, Windows cloud-placeholder guard.
- **Launch plumbing.** Start at login, rotating local crash log (256 KiB) with a
  Doctor check, unsigned multi-OS draft release workflow, CLI artifacts.
- **Tests and CI.** 77 Rust tests (parser, config migration, extraction, fuzzy
  relevance, ranking, MCP/CLI, images, crash log), `clippy -D warnings`, web and
  rust workflows, benchmark methodology.

### Changed
- Ranking now normalizes RRF before bonuses, bounds the depth penalty, clamps
  scores at zero, and drops weak semantic-only hits. This removes negative scores
  and shallow-but-irrelevant top results.
- The main window is files-only: search, category chips, results, preview. Stats
  and actions live in Settings.
- Grid view navigates by rows (Up/Down jump a row, Left/Right stay in the row).
- Default exclusions no longer skip PDF/Office/image formats now that extraction
  and OCR exist; a `config_version` migration cleans existing configs.
- Rewritten README, install/usage/FAQ/security/contributing docs, LICENSE.

### Fixed
- Document and image indexing were effectively disabled by legacy default
  exclusions.
- `state` reports the units' model id separately from the active query model.
- Embed queue no longer renders an inverse fill for queue depth.

## [0.2.0] — 2026-10-25

- Durable SQLite metadata with warm start; incremental crawl; identity by
  `(dev, ino)`; rename/delete handling; stale-document purge.
- Full query language and three-tier RRF fusion.
- PDF, Office, EPUB/ODT, RTF extraction; tesseract OCR.
- UI v2: `paper`/`void` themes, category chips, list/grid, command palette,
  preview pane, Settings with Doctor, Stats for Nerds, model download and factory
  reset.

## [0.1.0] — 2026-10-24

- Initial release: Rust engine (nucleo, Tantivy, jwalk, notify) and Tauri shell
  with a floating search window, tray, global shortcut and onboarding.
