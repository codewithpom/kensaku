# Usage

> This page documents **shipped behavior** in 1.0.16: plain-language intent, `/`
> commands, `@` scopes, facet counts, window modes, and settings. The visual
> Beta only downloads a model pack; it is not part of search yet.

## The window

Kensaku has two window modes, chosen during setup and switchable in
**Settings → General → Window**:

- **Launcher** (the setup default) — press <kbd>Alt</kbd>+<kbd>Space</kbd>
  (configurable in Settings) to toggle a floating search window. It hides on
  close, when it loses focus (clicking away), or on <kbd>Esc</kbd>, keeps the
  index warm in the tray, stays out of the taskbar/dock, and appears on every
  workspace.
- **Desktop app** — a normal decorated window in your taskbar/dock. Best if you
  treat Kensaku like any other app.

Either mode works from the same index. In launcher mode the global shortcut is
registered; in desktop app mode it is not, because the window is already in the
taskbar.

**Closing is separate from the mode.** `close_to_background` is on by default:
closing either window hides it, destroys the webview, and keeps the index warm,
so reopening is instant. Quit from the tray menu, or turn the setting off in
**Settings → General → Window**.

The main page is intentionally just **search + category chips + files**. Every
statistic and setting lives in Settings, so the search surface stays quiet.

- **Search bar** — type anything. Plain words search names and content.
- **Category chips** — All, Files, Folders, Images, Docs, Code, Media, Recent.
  They map to `type:` / `modified:` filters.
- **Results** — 52px rows or square-ish cards (minimum 168px). Selected row has an accent rail.
- **Preview** — appears when a result is selected and a query is active.
- **Status** — indexing progress is a thin line under the bar; no numbers on the
  main page.

## Keyboard

Every action has a key. Keys work from the search box unless noted.

| Key | Action |
|---|---|
| <kbd>Alt</kbd>+<kbd>Space</kbd> | show or hide the window (launcher mode; set this in Settings) |
| <kbd>/</kbd> | focus search, or open the command menu |
| <kbd>↑</kbd> <kbd>↓</kbd> | move selection (grid: by row) |
| <kbd>←</kbd> <kbd>→</kbd> | move within a grid row |
| <kbd>Space</kbd> | Quick Look: preview the selected file |
| <kbd>Enter</kbd> | open the selected file |
| <kbd>Shift</kbd>+<kbd>Enter</kbd> | show in folder |
| <kbd>⌘</kbd>/<kbd>Ctrl</kbd>+<kbd>C</kbd> | copy the selected path (outside the search box) |
| <kbd>⌘</kbd>/<kbd>Ctrl</kbd>+click, <kbd>Shift</kbd>+click, <kbd>⌘</kbd>/<kbd>Ctrl</kbd>+<kbd>A</kbd> | select several results (<kbd>Ctrl+A</kbd> works once you are in the list); <kbd>Esc</kbd> or an arrow key clears the selection |
| <kbd>⌘</kbd>/<kbd>Ctrl</kbd>+<kbd>I</kbd> | details for the selected file |
| <kbd>⌘</kbd>/<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>L</kbd> | toggle grid / list layout |
| <kbd>⌘,</kbd>/<kbd>Ctrl</kbd>+<kbd>,</kbd> | settings |
| <kbd>Esc</kbd> | close a dialog, clear the search, then (launcher mode) hide |

### Copy or move several results

Select more than one result, right-click one of them, and choose **Copy to…** or **Move to…**. Kensaku asks for a folder and never overwrites: a name that is already there becomes `name (1).ext`. Folders in the selection are skipped, and `Ctrl+A` selects the results loaded so far. Moves across drives copy and then delete. Moved files leave the list.

On Linux, set a shortcut for `kensaku --toggle` in your desktop settings. See [INSTALL.md](INSTALL.md#set-a-quick-shortcut).

## Query language

Everything is optional. Filters never replace the text; they narrow the set.

### Filters

| Syntax | Meaning |
|---|---|
| `type:pdf` | category: `image`, `doc`, `sheet`, `presentation`, `code`, `video`, `audio`, `archive`, `dir`, or an extension |
| `ext:rs` | exact extension |
| `in:~/Projects` | scope to a folder (quote paths with spaces) |
| `path:src-tauri` | substring match on the path |
| `modified:<7d` | changed within a window: `1h`, `7d`, `30d`, `7d..30d` |
| `size:>2mb` | size range: `>`, `<`, or `a..b` (`kb`, `mb`, `gb`) |
| `is:image`, `is:dir` | aliases for common categories |
| `lang:rust` | language family of the extension |
| `name:config` | match the filename only |
| `content:timeout` | match file contents only |
| `re:"fn \\w+"` | regular expression over content + filename |
| `-node_modules` | exclude term or folder |
| `"exact phrase"` | phrase |
| `mouse near[5] button` | terms within 5 words |
| `mode:exhaustive` | return every match, path-sorted (no ranked tiers) |

Examples:

```
type:pdf invoice
in:~/Projects kensaku indexer
modified:<7d config
size:>5mb is:image sunset
name:Cargo content:workspace
-node_modules docker error
re:"TODO|FIXME" ext:rs
```

### Plain language

Kensaku compiles common phrasing locally, without a model. Every inference is
shown as a removable chip above the results; removing a chip removes the words
that produced it. If the query already contains an explicit `filter:`, the
compiler leaves it alone.

| You type | It becomes |
|---|---|
| `latest resume` | `resume` + sort newest |
| `screenshots from last week` | `type:image modified:<7d` |
| `big videos in Projects` | `size:>100mb type:media in:Projects` |
| `tiny images` | `size:<1mb type:image` |
| `oldest invoices` | `invoices` + sort oldest |
| `biggest files` | `files` + sort largest |

Time words: `today`, `yesterday`, `last week`, `last month`, `last year`.
Size words: `big`/`large`/`huge` (>100 MB), `small`/`tiny` (<1 MB), and
`under 5 MB` / `over 10 MB`.

**Aliases.** Personal vocabulary lives in Settings → Search → Aliases. The
defaults include `resume → cv`, `invoice → receipt, bill`, and
`screenshot → capture`; aliases compile to a case-insensitive OR and appear as a
removable chip.

**Facet counts.** Category chips show counts from the server over the full
matched set, not just the visible page. The response also carries language,
place, time, and size counts for future facet UI.

### Home, pins, and saved searches

An empty search box shows pinned files, recent files, saved searches, and a few
suggested queries. Pin a result from the home rows; save the current query with
the bookmark button next to **Sort**. Saved searches store the query and sort and
appear on the home screen.

### Facets and pagination

Category chips show server-side counts over the full matched set. Click a chip to
include a category; **Alt-click** to exclude it. The selection is OR within the
category row and AND against the text query. When more ranked results exist, a
**Load more** control appends the next page.

Version groups appear as one row with a `N versions` badge (for example
`resume`, `resume_v2`, `resume_final` in the same folder). Near-identical
screenshots taken within ten minutes in the same folder also collapse into one
row; the badge count is the burst size. Existing images are grouped in the
background 50 at a time, and `/dupes` shows the same screenshot bursts.

### Scopes and sort

- Type `@` to pick a folder from the index; the choice inserts `in:"<path>"`.
- Type `/` for the command menu (filters, `/dupes`, `/reindex`, `/doctor`, `/settings`).
- When results are showing, the **Sort** control offers `Best match`, `Newest`,
  `Oldest`, `Largest`, and `Name`. Sorting is applied server-side before the
  result limit; `Best match` keeps the fused relevance order.

### How ranking works

Three tiers run in parallel — fuzzy filename, BM25 content, and static-vector
semantic — then fuse with reciprocal rank fusion. When exact tiers leave a
shortfall, a folded/trigram fallback catches look-alikes and OCR misreads
(`ContentFolded`). On top of that: exact/prefix
name bonuses, smooth recency, frecency from files you open, and a bounded depth
penalty. Semantic-only hits below a cosine floor are dropped, so a keyword query
doesn't return vaguely related documents.

Results are deterministic: score, then path.

## Settings

> **Setup (shipped):** the guided first run covers watched folders, exclusions,
> preferences (theme, interface language, window mode, shortcut), optional
> OCR/model downloads, and the first scan, ending with a Done summary before the
> main window opens. Settings shows current values and offers **About → Run setup
> again**; model/OCR downloads are acquired in setup, and Settings shows installed
> status only. Later launches stay search-first.

- **General** — theme (`light` / `dark`), interface language, global shortcut,
  max results, start at login, window mode, and **Keep running when closed**.
  The shortcut field records a combo when you click it and press the keys
  (Esc cancels).
- **Folders** — watched roots, add/remove, drag-and-drop.
- **Exclusions** — presets (Developer, Creative, All-round), directories,
  extensions.
- **Search** — thumbnails, grid/list layout, semantic toggle, OCR engine and
  language picker, crawl mode (`full` / `balanced` / `eco`), pause indexing,
  aliases, and the filter cheat sheet.
- **Meaning search** — active model, vector tier size and readiness, the f32
  table switch, embed-queue depth, **Re-embed**, and model files. Downloads are
  acquired in setup; use **About → Run setup again**.
- **Agents** — MCP config, one-click connect for Claude Desktop/Code and Cursor,
  the HTTP token, and CLI examples. External format extractors are config-only;
  see [EXTRACTORS.md](EXTRACTORS.md).
- **Health** — on-demand Doctor checks (config writability, DB integrity, index
  size, semantic units, watched roots, watcher health, inotify limit, model
  files, OCR, external extractors, crash log, version), Stats for Nerds
  (p50/p95/p99, index composition, ingestion phase/rate/queue, uptime, RSS,
  model ids), and the Beta section (visual pack download).
- **About** — version, update controls and release notes, report-an-issue, and
  **Reset everything**.

## Indexing behavior

- **Hidden entries** are skipped by default, along with common junk directories.
- **Sensitive paths and credentials** are default-denied: `.ssh`, `.gnupg`, `.aws`,
  `.kube`, `.docker`, `.password-store`, password-manager folders, `.env*`, and
  credential extensions (`.pem`, `.key`, `.pfx`, …).
- **Incremental** — unchanged files (same size + mtime) are not re-read.
- **Renames** are tracked by `(device, inode)`, so moves do not duplicate entries.
- **Startup** is search-first: the snapshot loads, then a stat-only reconcile runs
  in the background. This applies **after the first completed scan**; the guided
  first run waits for the initial scan and ends with a Done screen. The scanning
  screen shows files, documents, embedded units, a progress bar, and an ETA
  during metadata indexing and text extraction. Until it finishes the app reports
  how old the index is.
- **Unavailable roots** (unplugged drive, unmounted share) are preserved, not pruned.
- **Content policy** — text is extracted for files up to **100 MB**; up to
  **4 MB** of text is indexed per document, and up to **48** semantic units
  (overlapping windows) are embedded. `content_depth` defaults to `balanced`.
  Files above the size cap are still findable by name, path, type, size, and
  date. Extraction has a per-file timeout; dense images and scanned PDFs get up
  to 45 s.
- **Documents** — PDF, DOCX/PPTX/XLSX, EPUB/ODT, RTF; malformed files are skipped
  after their timeout. Scanned PDFs are OCR'd one page at a time instead of
  decoding the whole document up front.
- **Images** — OCR runs with PP-OCRv6 by default, on a bundled ONNX Runtime;
  the optional Indra engine (higher quality, ~27 MB pack) can be picked in
  setup or Settings, and Tesseract with language data is the fallback where
  ONNX cannot run (Intel Macs, or a `--no-default-features` build). Tesseract
  languages are selectable in Settings → Search. The setup flow offers
  `eng.traineddata` as an optional pack; Settings shows engine status and links
  to **Run setup again**. The toggle controls whether image text is indexed,
  and saving re-scans existing images. Thumbnails and dimensions are cached;
  near-duplicates use a perceptual hash, and screenshot bursts also compare OCR
  line overlap so a shared sidebar does not merge distinct pages.
- **Retries** — a file that fails extraction is retried at most three times and
  is not re-decoded forever. The counter resets when the file changes or the
  OCR settings change.
- **Cloud placeholders** (Windows OneDrive/iCloud, and macOS iCloud Drive via
  the APFS `SF_DATALESS` flag) are indexed by name only, so reading doesn't
  hydrate the file. Their sparse folders are listed but not descended into,
  and results-grid previews never download from the cloud.

## Agents

See [`AGENTS.md`](AGENTS.md). The short version:

```bash
kensaku-cli search "docker error" --json --limit 10
kensaku-cli state --json
kensaku-cli mcp
```

More agent surfaces:

```bash
# One-click connect (writes the client's config, with a .bak backup):
kensaku-cli connect claude-desktop --write   # or claude-code / cursor
kensaku-cli connect cursor                   # prints the JSON snippet instead

# Loopback HTTP/SSE transport for MCP clients (bearer token in
# <config>/http.token; printed once on startup):
kensaku-cli serve --port 8765
```

`serve` exposes `/mcp`, `/health`, `/search`, and `/events` only. There is no
`/ask` route and no chat endpoint; Kensaku does not bundle or call an LLM.

## Commands

Type `/` in the search box to open the command menu, then filter and press Enter:

| Command | Action |
|---|---|
| `/images` | Filter to images |
| `/pdf` | Filter to PDF files |
| `/docs` | Filter to documents |
| `/code` | Filter to source and config files |
| `/media` | Filter to audio and video |
| `/recent` | Files changed in the last 7 days |
| `/errors` | Search for error text |
| `/diagrams` | Search for diagrams |
| `/dupes` | Open the duplicate review view |
| `/reindex` | Re-scan watched folders |
| `/doctor` | Print an index health summary |
| `/settings` | Open Settings |
