# Configuration reference

Kensaku stores its settings in the OS config directory:

- Linux: `~/.config/kensaku/config.json`
- macOS: `~/Library/Application Support/kensaku/config.json`
- Windows: `%APPDATA%\kensaku\config.json`

The file is JSON. Keys are optional where noted; missing keys fall back to the
listed default. Settings changes are saved atomically, and an unknown or
invalid value falls back to the default instead of wedging the app.

Two overrides apply to the location (config file, metadata database, content
index, OCR state, everything else under the data directory):

1. **`KENSAKU_DATA_DIR`** environment variable. Highest precedence; useful for
   tests and for Nix/Flatpak or other sandboxed packaging that must pin the
   data directory.
2. **Portable marker file.** If a file named `portable` (or `kensaku.portable`)
   sits next to the executable, Kensaku keeps everything in `<exe_dir>/data`.
   This is what makes the Windows portable build self-contained: copy the
   folder, keep your index.

## HTTPS certificate trust

Model, OCR, and update downloads use the operating system's trusted CA
certificates, including corporate proxy CAs installed by your administrator.
To supply a custom trust bundle, set `SSL_CERT_FILE` to a PEM certificate bundle
or `SSL_CERT_DIR` to an OpenSSL-compatible certificate directory before starting
Kensaku. If either variable is set, only the specified locations are used instead
of the system store. Include all roots needed for your downloads in that bundle.
Restart Kensaku after changing these variables. Certificate and hostname
verification remain enabled; invalid or unreadable bundles do not disable TLS
verification.

## Keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `watched_dirs` | string[] | your home directory | Folders to index. |
| `excluded_dirs` | string[] | common build/cache names | Directory names to skip. |
| `excluded_extensions` | string[] | media/archive/binary exts | File extensions to skip. |
| `shortcut` | string | `Alt+Space` | Global toggle shortcut. |
| `max_results` | number | `50` | Maximum results per query. |
| `is_setup_complete` | bool | `false` | First-run setup was configured. |
| `first_scan_done` | bool | `false` | First full crawl finished. |
| `skip_hidden` | bool | `true` | Skip dotfiles/dotdirs. |
| `semantic_enabled` | bool | `true` | Enable the semantic (embedding) tier. |
| `semantic_model` | string | `static` | `static`, `smart`, or `none`. |
| `semantic_f32_table` | bool | `false` | Keep the static table in f32 instead of i8. |
| `crawl_mode` | string | `balanced` | `full`, `balanced`, or `eco`. Balanced uses idle priority; eco also paces on battery. |
| `pause_indexing` | bool | `false` | Pause background crawling; renames/deletes still apply. |
| `mcp_allow_reindex` | bool | `false` | Allow the MCP `reindex` write tool. |
| `ocr_enabled` | bool | `true` | Enable OCR for images. |
| `ocr_engine` | string | `accurate` | `accurate` (PP-OCRv6), `indra` (~27 MB pack), or `tesseract`. |
| `ocr_max_image_bytes` | number | `0` | Skip OCR above this size; `0` means no limit. |
| `ocr_languages` | string[] | `[]` | Tesseract language codes (e.g. `["eng", "hin"]`). Empty means English when installed. |
| `extractors` | object[] | `[]` | Opt-in external extractors; see [EXTRACTORS.md](EXTRACTORS.md). |
| `visual_search_beta_enabled` | bool | `false` | Show the Beta visual-pack download. The pack is not wired into search. |
| `content_depth` | string | `balanced` | `quick`, `balanced`, or `deep` for large documents. |
| `content_max_file_size_mb` | number | `100` | Largest file that gets text extraction; above this it is name/metadata-only. |
| `content_max_text_bytes` | number | `4000000` | Maximum extracted text per document (~4 MB). |
| `content_csv_head_rows` | number | `200` | Data rows indexed per CSV/TSV/Parquet file after the header; larger files are sampled like `head`. Rows past it are not searchable by value. |
| `content_max_semantic_units` | number | `48` | Maximum embedding chunks per document. |
| `agent_scoped_roots` | string[] | `[]` | MCP/CLI read-only scope roots. Empty means the whole index. |
| `include_ignored` | bool | `false` | Include files matched by `.gitignore` / `.cursorignore`. |
| `theme` | string | `dark` | `light` or `dark`. Fresh installs follow the OS theme before falling back to dark. |
| `language` | string\|null | `null` | UI language code (`en`, `zh`, `ru`); `null` auto-detects the system. |
| `aliases` | object[] | `resume → cv, curriculum vitae`; `invoice → receipt, bill`; `screenshot → screen shot, capture`; plus receipt, contract, photo | Personal vocabulary for plain-language queries. |
| `saved_searches` | object[] | `[]` | Saved queries shown on the home screen. |
| `pins` | string[] | `[]` | Pinned file paths shown on the home screen. |
| `show_thumbnails` | bool | `true` | Generate and show result thumbnails. |
| `results_view` | string | `grid` | `grid` or `list`. |
| `auto_update` | bool | `true` | Check GitHub for updates in the background (about every 12 hours). |
| `app_mode` | string | `launcher` | `launcher` (tray, shortcut, hides on blur) or `app` (normal taskbar window). |
| `close_to_background` | bool | `true` | Closing the window keeps Kensaku running in the tray. |
| `config_version` | number | internal | Schema version used for one-time migrations. Do not edit. |

## Example

```json
{
  "watched_dirs": ["~/Projects", "~/Documents"],
  "excluded_dirs": ["node_modules", ".git", "target", "dist"],
  "excluded_extensions": ["iso", "dmg", "vmdk"],
  "max_results": 50,
  "crawl_mode": "balanced",
  "semantic_enabled": true,
  "semantic_model": "static",
  "content_depth": "balanced",
  "content_max_file_size_mb": 100,
  "content_max_text_bytes": 4000000,
  "content_max_semantic_units": 48,
  "ocr_enabled": true,
  "ocr_engine": "accurate",
  "ocr_languages": ["eng"],
  "theme": "dark",
  "language": null,
  "results_view": "grid",
  "app_mode": "launcher",
  "close_to_background": true,
  "auto_update": true
}
```

## Migrations and rollback

`config_version` drives one-time migrations at load (for example, the old
`paper`/`void` theme names and the 20 MB/1 MB/20-unit content caps). A config
written by an older release keeps working; new keys get their defaults. If you
hand-edit a key and want to undo it, delete the key and restart.

The index has its own schema version. A content-schema upgrade keeps the old
index as `index.legacy-*`, creates a fresh one, and queues every file for one
content re-extraction. The old copy is safe to delete after the rebuild.

## What is not in the config

- Watched-folder exclusions are also enforced by `.gitignore` /
  `.cursorignore` unless `include_ignored` is true.
- Sensitive paths and credential extensions are denied in code, not via this
  file.
- Update assets and model packs are verified against pinned SHA-256 checksums;
  there is no setting to disable that.
