import type { TFunction } from "i18next";

/* Formatting helpers shared across components. Monospace-facing so that
   numbers tabulate cleanly in the status bar, stats and result rows.
   Locale-dependent helpers take the caller's `t` so the language can change
   without a reload and without importing the global i18n instance here. */

export function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return "0 B";
  const k = 1024;
  const sizes = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), sizes.length - 1);
  const v = bytes / Math.pow(k, i);
  return `${v >= 100 ? Math.round(v) : parseFloat(v.toFixed(1))} ${sizes[i]}`;
}

/** Active UI locale, as set on <html lang> by useLanguage. */
function uiLocale(): string {
  return (typeof document !== "undefined" && document.documentElement.lang) || "en-US";
}

export function formatComma(n: number): string {
  return n.toLocaleString(uiLocale());
}

export function formatRelativeTime(secs: number, t: TFunction): string {
  if (!secs) return "";
  const now = Math.floor(Date.now() / 1000);
  const diff = Math.max(0, now - secs);
  if (diff < 60) return t("time.justNow");
  if (diff < 3600) return t("time.minutesAgo", { count: Math.floor(diff / 60) });
  if (diff < 86400) return t("time.hoursAgo", { count: Math.floor(diff / 3600) });
  if (diff < 86400 * 30) return t("time.daysAgo", { count: Math.floor(diff / 86400) });
  if (diff < 86400 * 365) {
    return t("time.monthsAgo", { count: Math.floor(diff / (86400 * 30)) });
  }
  return t("time.yearsAgo", { count: Math.floor(diff / (86400 * 365)) });
}

export function formatDuration(secs: number, t: TFunction): string {
  if (!secs || secs <= 0) return t("time.duration.zero");
  if (secs < 60) return t("time.duration.seconds", { count: Math.round(secs) });
  const m = Math.floor(secs / 60);
  const h = Math.floor(m / 60);
  if (h > 0) return t("time.duration.hours", { hours: h, minutes: m % 60 });
  return t("time.duration.minutesSeconds", {
    minutes: m,
    seconds: Math.floor(secs % 60),
  });
}

/** Extracts the free-text search terms, dropping `filter:` tokens. */
export function queryTerms(query: string): string[] {
  return query
    .split(/\s+/)
    .map((t) => t.trim().toLowerCase())
    .filter((t) => t.length > 1 && !t.includes(":"));
}

/** Splits text into plain / match segments for subtle inline highlighting. */
export function highlightSegments(
  text: string,
  terms: string[],
): { text: string; match: boolean }[] {
  if (terms.length === 0) return [{ text, match: false }];
  const lower = text.toLowerCase();
  type Seg = { start: number; end: number };
  const ranges: Seg[] = [];
  for (const term of terms) {
    let idx = lower.indexOf(term);
    while (idx !== -1 && term.length > 0) {
      ranges.push({ start: idx, end: idx + term.length });
      idx = lower.indexOf(term, idx + term.length);
    }
  }
  if (ranges.length === 0) return [{ text, match: false }];
  ranges.sort((a, b) => a.start - b.start);
  // Merge overlaps.
  const merged: Seg[] = [ranges[0]];
  for (const r of ranges.slice(1)) {
    const last = merged[merged.length - 1];
    if (r.start <= last.end) last.end = Math.max(last.end, r.end);
    else merged.push(r);
  }
  const out: { text: string; match: boolean }[] = [];
  let pos = 0;
  for (const r of merged) {
    if (r.start > pos) out.push({ text: text.slice(pos, r.start), match: false });
    out.push({ text: text.slice(r.start, r.end), match: true });
    pos = r.end;
  }
  if (pos < text.length) out.push({ text: text.slice(pos), match: false });
  return out;
}

/** Formats a millisecond latency with a tailwind color class by bucket. */
export function latencyToken(ms: number): string {
  if (ms < 10) return "text-success";
  if (ms < 40) return "text-warning";
  return "text-danger";
}

export function basename(p: string): string {
  return p.split("/").filter(Boolean).pop() || p;
}

/** Parent folder for display: home collapsed to ~, long paths keep the tail. */
export function displayParent(path: string): string {
  const parts = path.replace(/\\/g, "/").split("/");
  parts.pop();
  let p = parts.join("/");
  p = p.replace(/^\/(Users|home)\/[^/]+/, "~").replace(/^[A-Z]:\/Users\/[^/]+/i, "~");
  const segs = p.split("/");
  return segs.length > 4 ? `${segs[0]}/…/${segs.slice(-2).join("/")}` : p || "/";
}
