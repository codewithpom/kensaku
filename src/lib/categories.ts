import { SearchResult } from "../types";

/* Category taxonomy shared by the result rows, grid cards and chip counts. */

export type Category =
  | "folder"
  | "image"
  | "doc"
  | "code"
  | "media"
  | "archive"
  | "file";

const IMAGE_EXT = new Set(["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "tiff", "heic", "avif"]);
const DOC_EXT = new Set([
  "pdf", "md", "txt", "doc", "docx", "ppt", "pptx", "odp", "key", /* docs+slides */
  "xls", "xlsx", "csv", "tsv", "ods", "numbers" /* sheets */,
  "rtf", "eml", "msg", "odt", "pages", "epub", "log",
]);
const CODE_EXT = new Set([
  "rs", "ts", "tsx", "js", "jsx", "py", "go", "c", "cpp", "h", "hpp", "java",
  "kt", "swift", "rb", "php", "lua", "zig", "nim", "html", "css", "scss",
  "json", "toml", "yaml", "yml", "xml", "sql", "sh", "bash", "fish", "conf",
]);
const MEDIA_EXT = new Set([
  "mp4", "mov", "avi", "mkv", "webm", "m4v", /* video */
  "mp3", "flac", "wav", "ogg", "m4a", "aac", "opus" /* audio */,
]);
const ARCHIVE_EXT = new Set(["zip", "tar", "gz", "rar", "7z", "bz2", "xz", "dmg", "iso"]);

/** Chip row definition. `token` maps to the query filter syntax. */
export interface CategoryChip {
  id: Category | "all" | "recent";
  token?: string; // query token appended, e.g. "type:image"
  stripTokens?: string[]; // tokens removed when selected (disambiguation)
}

export const CATEGORY_CHIPS: CategoryChip[] = [
  { id: "all" },
  { id: "file", token: "type:file" },
  { id: "folder", token: "type:dir" },
  { id: "image", token: "type:image" },
  { id: "doc", token: "type:doc" },
  { id: "code", token: "type:code" },
  { id: "media", token: "type:media" },
  { id: "recent", token: "modified:<7d" },
];

export function classify(result: { is_dir: boolean; filename: string }): Category {
  if (result.is_dir) return "folder";
  const ext = result.filename.split(".").pop()?.toLowerCase() || "";
  if (IMAGE_EXT.has(ext)) return "image";
  if (DOC_EXT.has(ext)) return "doc";
  if (CODE_EXT.has(ext)) return "code";
  if (MEDIA_EXT.has(ext)) return "media";
  if (ARCHIVE_EXT.has(ext)) return "archive";
  return "file";
}

/** Client-side counts per chip, for the badge in the chip row. */
export function categoryCounts(results: SearchResult[]): Record<string, number> {
  const counts: Record<string, number> = { all: results.length, recent: 0 };
  for (const r of results) {
    counts[classify(r)] = (counts[classify(r)] || 0) + 1;
    if (r.modified && Date.now() / 1000 - r.modified < 7 * 86400) {
      counts.recent = (counts.recent || 0) + 1;
    }
  }
  return counts;
}

/** Used by a `type:` chips to filter the flat result list client-side as well. */
export function matchesToken(result: SearchResult, token: string): boolean {
  if (token.startsWith("type:")) {
    const t = token.slice(5);
    const cat = classify(result);
    if (t === "file") return cat !== "folder" && cat !== "image" && cat !== "code" && cat !== "media";
    if (t === "dir") return cat === "folder";
    if (t === "image") return cat === "image";
    if (t === "doc") return cat === "doc";
    if (t === "code") return cat === "code";
    if (t === "media") return cat === "media";
    // Unknown backend type token — keep everything and let the server filter.
    return true;
  }
  if (token.startsWith("modified:<")) {
    const days = parseFloat(token.slice("modified:<".length).replace(/[^0-9.]/g, ""));
    if (result.modified && !Number.isNaN(days)) {
      return Date.now() / 1000 - result.modified < days * 86400;
    }
    return true;
  }
  return true;
}
