import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { Copy, FolderOpen, LoaderCircle, X } from "lucide-react";
import type { DuplicateGroup } from "../types";

/* Duplicate review: exact-content groups, screenshot bursts, and
   near-duplicate images.
   Read-only by design — open a file or reveal its folder; deletion is
   deliberately left to the OS/file manager. */

export function DuplicatesModal({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation("results");
  const [groups, setGroups] = useState<DuplicateGroup[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);

  useEffect(() => {
    invoke<DuplicateGroup[]>("find_duplicates", { maxDistance: 10, limit: 200 })
      .then(setGroups)
      .catch((e) => {
        setError(String(e));
        setGroups([]);
      });
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const copy = async (path: string) => {
    try {
      await navigator.clipboard.writeText(path);
      setCopied(path);
      window.setTimeout(() => setCopied((c) => (c === path ? null : c)), 1200);
    } catch {
      /* clipboard unavailable */
    }
  };

  const reveal = async (path: string) => {
    try {
      await invoke("open_folder", { path: path.replace(/[\\/][^\\/]+$/, "") });
    } catch (e) {
      console.error(e);
    }
  };

  const open = async (path: string) => {
    try {
      await invoke("open_file", { path });
    } catch (e) {
      console.error(e);
    }
  };

  const kindLabel = (kind: string) => {
    if (kind === "burst") return t("duplicates.kindBurst");
    if (kind === "content") return t("duplicates.kindExact");
    if (kind === "image") return t("duplicates.kindPerceptual");
    return kind;
  };

  return (
    <div className="sheet-bg absolute inset-0 z-50 flex flex-col bg-bg">
      <header className="flex h-14 shrink-0 items-center justify-between border-b border-line px-5">
        <div>
          <h1 className="text-[15px] font-semibold tracking-tight text-fg">{t("duplicates.title")}</h1>
          <p className="text-[12px] text-muted">{t("duplicates.subtitle")}</p>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="flex h-8 w-8 items-center justify-center rounded-control text-muted transition-colors hover:bg-accent-soft hover:text-fg"
          title={t("duplicates.close")}
        >
          <X className="h-4 w-4" />
        </button>
      </header>

      <main className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        {groups === null ? (
          <div className="flex h-full items-center justify-center gap-2 text-[12px] text-muted">
            <LoaderCircle className="h-4 w-4 animate-spin" />
            {t("duplicates.finding")}
          </div>
        ) : error ? (
          <p className="text-[12px] text-danger">{t("duplicates.loadFailed", { error })}</p>
        ) : groups.length === 0 ? (
          <div className="flex h-full flex-col items-center justify-center gap-1 text-center">
            <p className="text-[13px] font-medium text-fg">{t("duplicates.none")}</p>
            <p className="text-[12px] text-muted">{t("duplicates.noneHint")}</p>
          </div>
        ) : (
          <div className="mx-auto max-w-3xl space-y-3">
            {groups.map((g, i) => (
              <div key={`${g.kind}-${g.hash}-${i}`} className="surface">
                <div className="flex items-center justify-between gap-3 border-b border-line px-4 py-2.5">
                  <div className="flex items-center gap-2">
                    <span className="rounded-full bg-accent-soft px-2 py-0.5 font-mono text-[11px] text-muted">
                      {kindLabel(g.kind)}
                    </span>
                    <span className="text-[12px] font-medium text-fg">
                      {t("duplicates.files", { count: g.paths.length })}
                    </span>
                  </div>
                  <code className="truncate font-mono text-[11px] text-subtle">{g.hash}</code>
                </div>
                <ul>
                  {g.paths.map((p) => (
                    <li
                      key={p}
                      className="flex items-center justify-between gap-3 border-b border-line px-4 py-2 last:border-b-0"
                    >
                      <button
                        type="button"
                        onClick={() => open(p)}
                        title={t("duplicates.openFile")}
                        className="min-w-0 flex-1 truncate text-left font-mono text-[11px] text-fg transition-colors hover:text-accent"
                      >
                        {p}
                      </button>
                      <div className="flex shrink-0 items-center gap-1">
                        <button
                          type="button"
                          title={t("duplicates.copyPath")}
                          onClick={() => copy(p)}
                          className="flex h-6 w-6 items-center justify-center rounded text-subtle transition-colors hover:bg-accent-soft hover:text-fg"
                        >
                          <Copy className="h-3 w-3" />
                        </button>
                        <button
                          type="button"
                          title={t("duplicates.reveal")}
                          onClick={() => reveal(p)}
                          className="flex h-6 w-6 items-center justify-center rounded text-subtle transition-colors hover:bg-accent-soft hover:text-fg"
                        >
                          <FolderOpen className="h-3 w-3" />
                        </button>
                      </div>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>
        )}
        {copied && (
          <p className="fixed bottom-4 left-1/2 -translate-x-1/2 rounded-full bg-fg px-3 py-1 font-mono text-[11px] text-bg">
            {t("duplicates.copied")}
          </p>
        )}
      </main>
    </div>
  );
}