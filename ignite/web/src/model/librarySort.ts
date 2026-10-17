// Home/library display order (presentation only). Derived on every render from the projected
// canonical arrays + the sort mode + this browser's access ordinals; the inputs are never
// mutated and nothing derived here is stored, persisted or synchronized.
//
// * creation     — the canonical arrays exactly as given (folder order; each folder's and the
//                  top level's subsequence of library order).
// * alphabetical — folders by title; notebooks by title within their folder and at top level.
// * recent       — canonical folder order; within each folder and at top level, opened
//                  notebooks newest-first, then never-opened ones in library order.
// Folder membership never changes, and ties always fall back to canonical order.

import type { SnapshotFolder, SnapshotNotebook } from "./types";

export type SortMode = "creation" | "alphabetical" | "recent";

/** The cycle order of the sort button. */
export const SORT_MODES: readonly SortMode[] = ["creation", "alphabetical", "recent"];

export const SORT_LABELS: Record<SortMode, string> = {
  creation: "Creation Order",
  alphabetical: "Alphabetical Order",
  recent: "Recently Accessed",
};

export interface LibraryOrder {
  folders: readonly SnapshotFolder[];
  /** folder_id → its notebooks, in display order (every folder has an entry). */
  byFolder: ReadonlyMap<string, readonly SnapshotNotebook[]>;
  /** Notebooks in no (known) folder, in display order. */
  top: readonly SnapshotNotebook[];
}

const collator = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });

/** Locale-aware, case-insensitive title order. */
export function compareTitles(a: string, b: string): number {
  return collator.compare(a, b);
}

/** Stable sort by `cmp`, falling back to the input (canonical) order. */
function stableBy<T>(items: readonly T[], cmp: (a: T, b: T) => number): T[] {
  return items
    .map((item, i) => ({ item, i }))
    .sort((x, y) => cmp(x.item, y.item) || x.i - y.i)
    .map((x) => x.item);
}

export function orderLibrary(
  notebooks: readonly SnapshotNotebook[],
  folders: readonly SnapshotFolder[],
  membership: ReadonlyMap<string, string>,
  mode: SortMode,
  access: ReadonlyMap<string, number>,
): LibraryOrder {
  // Bucketing walks canonical library order, so each bucket is its canonical subsequence.
  const buckets = new Map<string, SnapshotNotebook[]>(folders.map((f) => [f.folder_id, []]));
  const top: SnapshotNotebook[] = [];
  for (const n of notebooks) {
    const f = membership.get(n.notebook_id);
    const list = f === undefined ? undefined : buckets.get(f);
    if (list) list.push(n);
    else top.push(n);
  }
  if (mode === "creation") return { folders, byFolder: buckets, top };

  const sortNotebooks =
    mode === "alphabetical"
      ? (list: readonly SnapshotNotebook[]) => stableBy(list, (a, b) => compareTitles(a.title, b.title))
      : (list: readonly SnapshotNotebook[]) =>
          // Unopened (0) sorts after every opened ordinal; equal ordinals keep library order.
          stableBy(list, (a, b) => (access.get(b.notebook_id) ?? 0) - (access.get(a.notebook_id) ?? 0));
  const byFolder = new Map<string, SnapshotNotebook[]>();
  for (const [id, list] of buckets) byFolder.set(id, sortNotebooks(list));
  return {
    folders: mode === "alphabetical" ? stableBy(folders, (a, b) => compareTitles(a.title, b.title)) : folders,
    byFolder,
    top: sortNotebooks(top),
  };
}
