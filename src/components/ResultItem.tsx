import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { SearchResult } from "../types";
import { HighlightedName } from "./EmptyState";
import { isThumbImage, useThumbnail } from "../lib/thumbnails";
import { formatBytes, formatRelativeTime } from "../lib/format";
import { HoverDetails } from "./HoverDetails";
import { TypeTile } from "./TypeTile";
import type { SelectMods } from "../lib/selection";

/* Result card. A 16:10 tile, then name and one meta line below it — the
   same card the home screen's recents use. Images fill the tile edge to
   edge; everything else is a typographic tile. The selected card gets the
   softened ink ring, which is the only color on the grid. */

export type ResultView = "grid" | "list";

interface ResultItemProps {
  result: SearchResult;
  query: string;
  index: number;
  isSelected: boolean;
  view: ResultView;
  onSelect: (mods?: SelectMods) => void;
  onOpen: () => void;
  onReveal?: () => void;
  onCopyPath?: () => void;
  onContextMenu?: (x: number, y: number) => void;
  showThumbs?: boolean;
}

export const ResultItem = React.memo<ResultItemProps>(
  ({ result, query, index, isSelected, onSelect, onOpen, onContextMenu, showThumbs }) => {
    const { t } = useTranslation("common");
    const imageEligible = Boolean(showThumbs) && !result.is_dir && isThumbImage(result.filename);
    const { src: thumbSrc, failed: thumbFailed } = useThumbnail(result.path, imageEligible);
    // Turning thumbnails off must also drop images that were already loaded.
    const hasThumb = imageEligible && Boolean(thumbSrc) && !thumbFailed;
    const [hovered, setHovered] = useState(false);

    const meta = result.is_dir
      ? `${t("units.folder")} · ${formatRelativeTime(result.modified, t)}`
      : `${formatBytes(result.file_size)} · ${formatRelativeTime(result.modified, t)}`;

    return (
      <button
        type="button"
        onMouseDown={(e) => {
          if (e.shiftKey) e.preventDefault();
        }}
        onClick={(e) => onSelect({ toggle: e.ctrlKey || e.metaKey, range: e.shiftKey })}
        onDoubleClick={onOpen}
        onMouseEnter={() => setHovered(true)}
        onFocus={() => setHovered(true)}
        onContextMenu={(e) => {
          if (!onContextMenu) return;
          e.preventDefault();
          onSelect({ contextMenu: true });
          onContextMenu(e.clientX, e.clientY);
        }}
        draggable
        onDragStart={(e) => {
          e.dataTransfer.effectAllowed = "copy";
          e.dataTransfer.setData("text/plain", result.path);
          e.dataTransfer.setData("text/uri-list", `file://${result.path}`);
        }}
        aria-selected={isSelected}
        title={result.path}
        style={{ ["--stagger-delay" as string]: `${Math.min(index, 11) * 28}ms` }}
        className="group card-in flex h-full w-full flex-col text-left outline-none"
      >
        <span
          className={`surface relative block aspect-[16/10] w-full overflow-hidden transition-[transform,box-shadow] duration-[280ms] [transition-timing-function:var(--ease)] group-hover:-translate-y-[3px] group-hover:[box-shadow:var(--panel-edge),var(--shadow-float)] ${
            isSelected ? "outline-2 outline-offset-[3px] outline-[color-mix(in_srgb,var(--ink)_45%,transparent)]" : ""
          }`}
        >
          {hasThumb ? (
            <img
              src={thumbSrc!}
              alt=""
              draggable={false}
              className="fade-in h-full w-full object-cover transition-transform duration-[520ms] [transition-timing-function:var(--ease)] group-hover:scale-[1.04]"
            />
          ) : (
            <TypeTile filename={result.filename} isDir={result.is_dir} snippet={result.snippet} query={query} />
          )}
          <HoverDetails result={result} active={hovered} />
        </span>

        <span className="flex min-w-0 flex-col px-0.5 pt-2">
          <span className="truncate text-[13px] font-medium leading-[18px] text-fg">
            <HighlightedName text={result.filename} query={query} />
          </span>
          <span className="t-meta truncate text-subtle">{meta}</span>
        </span>
      </button>
    );
  },
  (prev, next) =>
    prev.result.path === next.result.path &&
    prev.result.snippet === next.result.snippet &&
    prev.result.match_type === next.result.match_type &&
    prev.result.group_count === next.result.group_count &&
    prev.result.file_size === next.result.file_size &&
    prev.result.modified === next.result.modified &&
    prev.result.is_dir === next.result.is_dir &&
    prev.isSelected === next.isSelected &&
    prev.query === next.query &&
    prev.index === next.index &&
    prev.showThumbs === next.showThumbs,
);
