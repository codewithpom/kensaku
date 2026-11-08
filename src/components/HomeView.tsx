import type { HomeData } from "../types";
import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { formatRelativeTime } from "../lib/format";
import { isThumbImage, useThumbnail } from "../lib/thumbnails";
import { TypeTile } from "./TypeTile";

/* Home. The field above is the invitation; below it, what you touched
   last, as tiles. Saved searches sit on one quiet line above. With nothing
   recent, a single serif line and a few starting searches. */

function RecentTile({
  item,
  index,
  showThumbs,
  focused,
  onOpen,
}: {
  item: HomeData["recent"][number];
  index: number;
  showThumbs?: boolean;
  focused?: boolean;
  onOpen: () => void;
}) {
  const eligible = Boolean(showThumbs) && !item.is_dir && isThumbImage(item.filename);
  const { src, failed } = useThumbnail(item.path, eligible);
  const name = item.filename || item.path;
  const ref = useRef<HTMLButtonElement>(null);
  const { t } = useTranslation("home");
  const { t: tCommon } = useTranslation("common");
  /* Keyboard walks go below the fold: keep the focused tile in sight. */
  useEffect(() => {
    if (focused) ref.current?.scrollIntoView({ block: "nearest" });
  }, [focused]);
  return (
    <button
      ref={ref}
      type="button"
      onClick={onOpen}
      title={item.path}
      data-index={index}
      aria-current={focused ? "true" : undefined}
      style={{ ["--stagger-delay" as string]: `${Math.min(index, 11) * 32}ms` }}
      className="group card-in flex min-w-0 flex-col text-left outline-none"
    >
      <span
        className={`surface relative block aspect-[16/10] w-full overflow-hidden transition-[transform,box-shadow] duration-[280ms] [transition-timing-function:var(--ease)] group-hover:-translate-y-[3px] group-hover:[box-shadow:var(--panel-edge),var(--shadow-float)] group-focus-visible:outline-2 group-focus-visible:outline-offset-[3px] group-focus-visible:outline-[color-mix(in_srgb,var(--ink)_45%,transparent)] ${
          focused ? "outline-2 outline-offset-[3px] outline-[color-mix(in_srgb,var(--ink)_45%,transparent)]" : ""
        }`}
      >
        {eligible && src && !failed ? (
          <img src={src} alt="" draggable={false} className="fade-in h-full w-full object-cover transition-transform duration-[520ms] [transition-timing-function:var(--ease)] group-hover:scale-[1.04]" />
        ) : (
          <TypeTile filename={name} isDir={item.is_dir} />
        )}
      </span>
      <span className="flex min-w-0 flex-col px-0.5 pt-2">
        <span className="truncate text-[13px] font-medium leading-[18px] text-fg">{name}</span>
        <span className="t-meta truncate text-subtle">
          {item.last_open > 0 ? t("opened", { time: formatRelativeTime(item.last_open, tCommon) }) : formatRelativeTime(item.modified, tCommon)}
        </span>
      </span>
    </button>
  );
}

export function HomeView({
  data,
  recentQueries: propRecentQueries,
  onClearRecentQueries,
  onSearch,
  onOpen,
  showThumbs,
  focusIndex,
}: {
  data: HomeData | null;
  recentQueries?: string[];
  onClearRecentQueries?: () => void;
  onSearch: (query: string) => void;
  onOpen: (path: string) => void;
  showThumbs?: boolean;
  /** Keyboard-walk selection; mirrors the launcher's result selection. */
  focusIndex?: number | null;
}) {
  const { t } = useTranslation("home");
  if (!data) return null;
  const recent = data.recent.slice(0, 18);
  const saved = data.saved_searches ?? [];
  const recentQueries = (propRecentQueries ?? data.recent_queries ?? []).filter(Boolean);
  const suggestions = [
    t("suggestions.resume"),
    t("suggestions.screenshots"),
    t("suggestions.videos"),
    t("suggestions.invoice"),
  ];

  if (recent.length === 0) {
    if (recentQueries.length === 0) {
      return (
        <div className="flex flex-1 flex-col items-start justify-end gap-4 px-4 pb-8">
          <p className="t-moment rise-in max-w-md text-muted">{t("emptyHint")}</p>
          <div className="flex flex-wrap gap-1.5">
            {suggestions.map((q, i) => (
              <button
                key={q}
                type="button"
                onClick={() => onSearch(q)}
                style={{ ["--stagger-delay" as string]: `${120 + i * 50}ms` }}
                className="btn btn-secondary rise-in h-8 !rounded-full"
              >
                {q}
              </button>
            ))}
          </div>
        </div>
      );
    }

    return (
      <div className="flex flex-1 flex-col items-start justify-end gap-4 px-4 pb-8">
        <div className="flex w-full flex-col gap-2">
          <div className="flex items-center justify-between">
            <span className="t-meta text-subtle">{t("recentSearches")}</span>
            {onClearRecentQueries && (
              <button
                type="button"
                onClick={onClearRecentQueries}
                className="t-meta text-subtle transition-colors hover:text-fg cursor-pointer"
              >
                {t("clearRecentSearches")}
              </button>
            )}
          </div>
          <div className="flex flex-wrap gap-1.5">
            {recentQueries.map((q, i) => (
              <button
                key={q}
                type="button"
                onClick={() => onSearch(q)}
                title={q}
                style={{ ["--stagger-delay" as string]: `${i * 35}ms` }}
                className="btn btn-secondary rise-in h-8 max-w-[260px] truncate !rounded-full"
              >
                {q}
              </button>
            ))}
          </div>
        </div>
        {recentQueries.length < 3 && (
          <div className="flex w-full flex-col gap-2 pt-1">
            <p className="t-moment rise-in max-w-md text-muted">{t("emptyHint")}</p>
            <div className="flex flex-wrap gap-1.5">
              {suggestions.map((q, i) => (
                <button
                  key={q}
                  type="button"
                  onClick={() => onSearch(q)}
                  title={q}
                  style={{ ["--stagger-delay" as string]: `${(recentQueries.length + i) * 35 + 80}ms` }}
                  className="btn btn-secondary rise-in h-8 max-w-[260px] truncate !rounded-full opacity-70 hover:opacity-100"
                >
                  {q}
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    );
  }

  return (
    <div className="px-3.5 pb-4 pt-1.5">
      {saved.length > 0 && (
        <div className="mb-4 flex flex-wrap items-baseline gap-x-5 gap-y-2">
          <span className="t-meta text-subtle">{t("saved")}</span>
          {saved.map((s) => (
            <button
              key={s.name}
              type="button"
              onClick={() => onSearch(s.query)}
              className="text-[13px] text-muted underline decoration-line-strong decoration-1 underline-offset-4 transition-colors hover:text-fg hover:decoration-ink"
            >
              {s.name}
            </button>
          ))}
        </div>
      )}
      {recentQueries.length > 0 && (
        <div className="mb-4 flex flex-wrap items-baseline justify-between gap-y-2">
          <div className="flex flex-wrap items-baseline gap-x-5 gap-y-2">
            <span className="t-meta text-subtle">{t("recentSearches")}</span>
            {recentQueries.map((q) => (
              <button
                key={q}
                type="button"
                onClick={() => onSearch(q)}
                title={q}
                className="max-w-[240px] truncate text-[13px] text-muted underline decoration-line-strong decoration-1 underline-offset-4 transition-colors hover:text-fg hover:decoration-ink"
              >
                {q}
              </button>
            ))}
          </div>
          {onClearRecentQueries && (
            <button
              type="button"
              onClick={onClearRecentQueries}
              className="t-meta text-subtle transition-colors hover:text-fg"
            >
              {t("clearRecentSearches")}
            </button>
          )}
        </div>
      )}
      <p className="t-meta mb-2.5 px-0.5 text-subtle">{t("recent")}</p>
      <div className="grid gap-x-3 gap-y-4" style={{ gridTemplateColumns: "repeat(auto-fill, minmax(168px, 1fr))" }}>
        {recent.map((r, i) => (
          <RecentTile
            key={r.path}
            item={r}
            index={i}
            showThumbs={showThumbs}
            focused={focusIndex === i}
            onOpen={() => onOpen(r.path)}
          />
        ))}
      </div>
    </div>
  );
}
