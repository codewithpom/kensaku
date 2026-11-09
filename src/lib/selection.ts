/** How a row was picked: Ctrl or Cmd toggles, Shift extends, a context menu keeps a multi-selection. */
export interface SelectMods {
  toggle?: boolean;
  range?: boolean;
  contextMenu?: boolean;
}

/** Returns a copy of `selected` with `path` added, or removed if it was there. */
export function toggle(selected: ReadonlySet<string>, path: string): Set<string> {
  const next = new Set(selected);
  if (!next.delete(path)) next.add(path);
  return next;
}

/** Paths from `anchor` to `target` inclusive, in either direction. */
export function rangeSelect(paths: readonly string[], anchor: number, target: number): Set<string> {
  const from = Math.max(0, Math.min(anchor, target));
  const to = Math.min(paths.length - 1, Math.max(anchor, target));
  return new Set(paths.slice(from, to + 1));
}
