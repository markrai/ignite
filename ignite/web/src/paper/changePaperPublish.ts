// Change Paper: publish template blob once, then N independent SetPageBackground ops.
//
// Patterned paper requires a successful PUT (same as CreateNotebook). Blank skips the network.
// All-pages is a prefix-safe sequence: already-queued pages stay changed if later pages fail
// or the user cancels. No notebook-level template field.

import type { BlobApi } from "../api/blobApi";
import type { BackgroundRef, SnapshotNotebook, SnapshotPage } from "../model/types";
import type { SetPageBackgroundIntent } from "../replica/store";
import {
  knownTemplateBlobShas,
  pagesIncludeCustomBackground,
  type PageTemplate,
} from "./paperTemplates";
import {
  publishTemplateBackground,
  TemplatePublishError,
  type OnlineProbe,
} from "./publishTemplateBackground";
import type { ReadyReplica } from "./createNotebookPublish";

export type PaperChangeScope = "this" | "all";

/** Guards overlapping Change paper confirmation and preserves in-flight cancellation. */
export class PaperChangeRun {
  busy = false;
  cancelled = false;

  /** @returns false if a run is already in progress (ignore overlapping confirmation). */
  tryBegin(): boolean {
    if (this.busy) return false;
    this.busy = true;
    this.cancelled = false;
    return true;
  }

  cancel(): void {
    this.cancelled = true;
  }

  finish(): void {
    this.busy = false;
  }

  isCancelled(): boolean {
    return this.cancelled;
  }
}

export function paperChangeIncompleteNotice(applied: number, total: number, failed: boolean): string {
  if (applied === 0) {
    return failed
      ? "Couldn’t change the paper on this device. Nothing was changed."
      : "No pages were changed.";
  }
  const stay = "Already-changed pages stay changed.";
  return failed
    ? `Changed ${applied} of ${total} pages. Some pages could not be changed. ${stay}`
    : `Changed ${applied} of ${total} pages. ${stay}`;
}

const GATE_OFF = "This server does not accept Change paper yet. Nothing was changed.";
const GATE_UNREACHABLE = "Change paper needs a connection to a server that allows it. Nothing was changed.";

export function changePaperTargets(
  notebook: SnapshotNotebook,
  scope: PaperChangeScope,
  currentPageId: string | null,
): SnapshotPage[] {
  if (scope === "all") return [...notebook.pages];
  const current = notebook.pages.find((p) => p.page_id === currentPageId);
  return [current ?? notebook.pages[0]!];
}

export async function targetsIncludeCustomPaper(
  pages: readonly SnapshotPage[],
): Promise<boolean> {
  return pagesIncludeCustomBackground(pages, await knownTemplateBlobShas());
}

export async function commitSetPageBackgroundOn(
  ready: ReadyReplica,
  intent: SetPageBackgroundIntent,
): Promise<boolean> {
  let operationId: string;
  try {
    operationId = ready.mintId("operation");
  } catch {
    return false;
  }
  const op = await ready.coordinator.commit({
    kind: "set_page_background",
    operation_id: operationId,
    notebook_id: intent.notebook_id,
    page_id: intent.page_id,
    set_page_background: intent,
  });
  return op !== null;
}

/**
 * PUT patterned bytes (once), then queue one SetPageBackground per target page.
 * `isCancelled` is checked between pages; already-committed pages are left changed.
 */
export async function publishThenChangePaper(opts: {
  template: PageTemplate;
  notebook: SnapshotNotebook;
  scope: PaperChangeScope;
  currentPageId: string | null;
  blobApi: BlobApi;
  stillValid: () => boolean;
  commit: (intent: SetPageBackgroundIntent) => Promise<boolean>;
  onProgress?: (done: number, total: number) => void;
  isCancelled?: () => boolean;
  isOnline?: OnlineProbe;
  /** Fresh library-head probe. Required: never journal against an unactivated or unreachable server. */
  setPageBackgroundAccepted: () => Promise<boolean>;
}): Promise<{ ok: boolean; notice: string | null; applied: number }> {
  try {
    if (!(await opts.setPageBackgroundAccepted())) {
      return { ok: false, notice: GATE_OFF, applied: 0 };
    }
  } catch {
    return { ok: false, notice: GATE_UNREACHABLE, applied: 0 };
  }
  const targets = changePaperTargets(opts.notebook, opts.scope, opts.currentPageId);
  if (targets.length === 0) {
    return { ok: false, notice: "Couldn’t change the paper on this device. Nothing was changed.", applied: 0 };
  }
  let background: BackgroundRef;
  try {
    ({ background } = await publishTemplateBackground(opts.template, opts.blobApi, opts.isOnline, "change"));
  } catch (e) {
    return {
      ok: false,
      notice: e instanceof TemplatePublishError ? e.message : "Couldn’t prepare the paper background. Nothing was changed.",
      applied: 0,
    };
  }
  if (!opts.stillValid()) {
    return { ok: false, notice: "Couldn’t change the paper on this device. Nothing was changed.", applied: 0 };
  }
  const total = targets.length;
  opts.onProgress?.(0, total);
  let applied = 0;
  for (const page of targets) {
    if (opts.isCancelled?.()) {
      return {
        ok: applied > 0,
        notice: paperChangeIncompleteNotice(applied, total, false),
        applied,
      };
    }
    if (!opts.stillValid()) {
      return {
        ok: applied > 0,
        notice: paperChangeIncompleteNotice(applied, total, true),
        applied,
      };
    }
    const queued = await opts.commit({
      type: "set_page_background",
      notebook_id: opts.notebook.notebook_id,
      page_id: page.page_id,
      background,
    });
    if (!queued) {
      return {
        ok: applied > 0,
        notice: paperChangeIncompleteNotice(applied, total, true),
        applied,
      };
    }
    applied += 1;
    opts.onProgress?.(applied, total);
  }
  return { ok: applied === total, notice: applied === total ? null : paperChangeIncompleteNotice(applied, total, true), applied };
}

/** Queue one SetPageBackground on a specific ready replica (not a React-captured closure). */
export async function changePaperOnReady(
  ready: ReadyReplica,
  opts: Omit<Parameters<typeof publishThenChangePaper>[0], "blobApi" | "commit"> & {
    setPageBackgroundAccepted: () => Promise<boolean>;
  },
): Promise<{ ok: boolean; notice: string | null; applied: number }> {
  return publishThenChangePaper({
    ...opts,
    blobApi: ready.blobApi,
    commit: (intent) => commitSetPageBackgroundOn(ready, intent),
  });
}
