// Replica-local view selection (never synchronized). Preserved across refreshed snapshots by
// identity; when the selected page/notebook disappeared, fall back to the nearest survivor.

import type { SnapshotNotebook, SnapshotPage, SyncPageLink, SyncSnapshotV1 } from "./types";

export interface Selection {
  notebookId: string;
  pageId: string;
}

export interface Resolved {
  notebookIndex: number;
  pageIndex: number;
  notebook: SnapshotNotebook;
  page: SnapshotPage;
}

export function resolve(snapshot: SyncSnapshotV1, sel: Selection | null): Resolved | null {
  if (snapshot.notebooks.length === 0) return null;
  let ni = sel ? snapshot.notebooks.findIndex((n) => n.notebook_id === sel.notebookId) : -1;
  if (ni < 0) ni = 0;
  const notebook = snapshot.notebooks[ni]!;
  let pi = sel ? notebook.pages.findIndex((p) => p.page_id === sel.pageId) : -1;
  if (pi < 0) pi = 0;
  return { notebookIndex: ni, pageIndex: pi, notebook, page: notebook.pages[pi]! };
}

/**
 * Selection for `next` given the previous selection made against `prev`.
 * * page still exists (anywhere in its notebook) → keep it;
 * * page deleted → the page now at its old index (clamped), i.e. its successor or the new last;
 * * notebook deleted → the notebook now at its old index (clamped), first page.
 */
export function preserveSelection(
  prev: SyncSnapshotV1 | null,
  sel: Selection | null,
  next: SyncSnapshotV1,
): Selection | null {
  if (next.notebooks.length === 0) return null;
  const first = (n: SnapshotNotebook): Selection => ({ notebookId: n.notebook_id, pageId: n.pages[0]!.page_id });
  if (!sel) return first(next.notebooks[0]!);
  const nb = next.notebooks.find((n) => n.notebook_id === sel.notebookId);
  const prevNbIndex = prev ? prev.notebooks.findIndex((n) => n.notebook_id === sel.notebookId) : -1;
  if (!nb) {
    const i = Math.min(Math.max(prevNbIndex, 0), next.notebooks.length - 1);
    return first(next.notebooks[i]!);
  }
  if (nb.pages.some((p) => p.page_id === sel.pageId)) return sel;
  const prevNb = prev?.notebooks.find((n) => n.notebook_id === sel.notebookId);
  const prevPageIndex = prevNb ? prevNb.pages.findIndex((p) => p.page_id === sel.pageId) : -1;
  const i = Math.min(Math.max(prevPageIndex, 0), nb.pages.length - 1);
  return { notebookId: nb.notebook_id, pageId: nb.pages[i]!.page_id };
}

/** Same copy as Kindle `MISSING_LINK_LABEL` when the destination is gone. */
export const MISSING_LINK_LABEL = "Linked page no longer exists";

/** Live jump target, or `null` if the destination notebook/page is gone. */
export function resolveLinkTarget(snapshot: SyncSnapshotV1, link: SyncPageLink): Selection | null {
  const notebook = snapshot.notebooks.find((n) => n.notebook_id === link.target_notebook_id);
  if (!notebook?.pages.some((p) => p.page_id === link.target_page_id)) return null;
  return { notebookId: notebook.notebook_id, pageId: link.target_page_id };
}

/** Hover/ARIA label: `"Title - Page N"` (Kindle jump copy without the Jump wrapper), or missing. */
export function linkJumpLabel(snapshot: SyncSnapshotV1, link: SyncPageLink): string {
  const notebook = snapshot.notebooks.find((n) => n.notebook_id === link.target_notebook_id);
  const i = notebook?.pages.findIndex((p) => p.page_id === link.target_page_id) ?? -1;
  if (!notebook || i < 0) return MISSING_LINK_LABEL;
  return `${notebook.title} - Page ${i + 1}`;
}

/** Step within the selected notebook; `null` at either end. */
export function stepPage(snapshot: SyncSnapshotV1, sel: Selection, delta: number): Selection | null {
  const r = resolve(snapshot, sel);
  if (!r) return null;
  const i = r.pageIndex + delta;
  if (i < 0 || i >= r.notebook.pages.length) return null;
  return { notebookId: r.notebook.notebook_id, pageId: r.notebook.pages[i]!.page_id };
}
