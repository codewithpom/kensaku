import React from "react";
import { useTranslation } from "react-i18next";
import { HighlightedName } from "./EmptyState";
import { isThumbImage, useThumbnail } from "../lib/thumbnails";
import { displayParent, formatBytes, formatRelativeTime } from "../lib/format";
import { extOf } from "./TypeTile";
import type { SelectMods } from "../lib/selection";

/* One file per row, fixed height. Used for list results and recents so the
   two lists look identical. Mark, name over parent folder, size, age. */

export interface FileRowItem {
  path: string;
  filename: string;
  file_size: number;
  modified: number;
  is_dir: boolean;
}

export const FILE_ROW_HEIGHT = 52;

export const FileRow = React.memo(function FileRow({
  item,
  query = "",
  selected = false,
  showThumbs = false,
  onSelect,
  onOpen,
  onContextMenu,
}: {
  item: FileRowItem;
  query?: string;
  selected?: boolean;
  showThumbs?: boolean;
  onSelect: (mods?: SelectMods) => void;
  onOpen: () => void;
  onContextMenu?: (x: number, y: number) => void;
}) {
  const eligible = showThumbs && !item.is_dir && isThumbImage(item.filename);
  const { t } = useTranslation("common");
  const { src, failed } = useThumbnail(item.path, eligible);
  const thumb = eligible && src && !failed ? src : null;
  const ext = extOf(item.filename, item.is_dir);

  return (
    <div
      role="row"
      aria-selected={selected}
      title={item.path}
      onMouseDown={(e) => {
        if (e.shiftKey) e.preventDefault();
      }}
      onClick={(e) => onSelect({ toggle: e.ctrlKey || e.metaKey, range: e.shiftKey })}
      onDoubleClick={onOpen}
      onContextMenu={(e) => {
        if (!onContextMenu) return;
        e.preventDefault();
        onSelect({ contextMenu: true });
        onContextMenu(e.clientX, e.clientY);
      }}
      draggable
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = "copy";
        e.dataTransfer.setData("text/plain", item.path);
        e.dataTransfer.setData("text/uri-list", `file://${item.path}`);
      }}
      style={{ height: FILE_ROW_HEIGHT }}
      className={`group relative flex w-full cursor-default items-center gap-2.5 rounded-control px-2.5 transition-colors duration-150 ${
        selected ? "bg-accent-soft" : "hover:bg-accent-soft"
      }`}
    >
      {selected && <span className="absolute left-0 top-1/2 h-5 w-[2px] -translate-y-1/2 rounded-full bg-ink/40" />}
      {thumb ? (
        <img src={thumb} alt="" draggable={false} className="h-8 w-8 shrink-0 rounded-control object-cover shadow-[var(--panel-edge)]" />
      ) : (
        <span className="surface flex h-8 w-8 shrink-0 items-center justify-center rounded-control font-mono text-[10px] font-medium text-muted">
          {item.is_dir ? "/" : ext.slice(0, 4)}
        </span>
      )}
      <div className="min-w-0 flex-1">
        <p className="truncate text-[13px] font-medium leading-[18px] text-fg">
          <HighlightedName text={item.filename} query={query} />
        </p>
        <p className="truncate text-[12px] leading-[16px] text-subtle">{displayParent(item.path)}</p>
      </div>
      <span className="t-meta w-16 shrink-0 text-right text-subtle">
        {item.is_dir ? "—" : formatBytes(item.file_size)}
      </span>
      <span className="t-meta w-16 shrink-0 text-right text-subtle">{formatRelativeTime(item.modified, t)}</span>
    </div>
  );
});
