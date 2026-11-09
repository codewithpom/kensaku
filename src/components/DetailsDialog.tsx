import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { SearchResult } from "../types";
import { formatBytes, formatDateTime, formatRelativeTime } from "../lib/format";

/**
 * "How we found it" view. Opened from the right-click menu, never inline.
 * Shows the match type, the score, and the facts about the file.
 */
export function DetailsDialog({
  result,
  onClose,
}: {
  result: SearchResult;
  onClose: () => void;
}) {
  const { t, i18n } = useTranslation("details");
  const { t: tCommon } = useTranslation("common");
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  const rows: [string, string][] = [
    [t("location"), result.path],
    [t("size"), formatBytes(result.file_size)],
    [
      t("modified"),
      result.modified
        ? `${formatDateTime(result.modified, i18n.language)} · ${formatRelativeTime(result.modified, tCommon)}`
        : "",
    ],
    [t("matchedBy"), matchLabel(result.match_type, t)],
    [t("score"), result.score.toFixed(2)],
  ];

  return (
    <div
      className="fade-in fixed inset-0 z-[80] flex items-center justify-center bg-scrim p-6 backdrop-blur-[6px]"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        role="dialog"
        aria-label={t("title")}
        className="elevated overlay-in w-full max-w-[440px] rounded-sheet p-6"
      >
        <p className="t-title break-words text-fg">{result.filename}</p>
        <dl className="mt-6 divide-y divide-line">
          {rows.map(([label, value]) => (
            <div key={label} className="flex items-baseline justify-between gap-6 py-2.5">
              <dt className="shrink-0 text-[13px] text-muted">{label}</dt>
              <dd className="t-meta min-w-0 truncate text-right text-fg">
                {value}
              </dd>
            </div>
          ))}
        </dl>
        <div className="mt-6 flex justify-end">
          <button
            type="button"
            onClick={onClose}
            className="btn btn-primary h-9 px-5"
          >
            {t("done")}
          </button>
        </div>
      </div>
    </div>
  );
}

function matchLabel(
  matchType: SearchResult["match_type"],
  t: TFunction<"details">,
): string {
  switch (matchType) {
    case "FileName":
      return t("match.fileName");
    case "Content":
      return t("match.content");
    case "ContentFolded":
      return t("match.contentFolded");
    case "Semantic":
      return t("match.semantic");
    case "Both":
      return t("match.both");
    default:
      return String(matchType);
  }
}
