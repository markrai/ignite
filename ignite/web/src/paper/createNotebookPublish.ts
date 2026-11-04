// Patterned CreateNotebook after blob PUT (Slice 6B).
//
// The Viewer captures the initiating ready replica + session before upload. After PUT resolves,
// creation proceeds only if that same identity is still active and the Viewer is mounted —
// otherwise a late upload must not mint or queue CreateNotebook on a stale/switched session.

import type { BlobApi } from "../api/blobApi";
import type { BackgroundRef } from "../model/types";
import type { SyncCoordinator } from "../replica/coordinator";
import type { FolderIntent, StructureIntent } from "../replica/store";
import type { LibrarySession } from "../state/librarySession";
import {
  publishTemplateBackground,
  TemplatePublishError,
  type OnlineProbe,
} from "./publishTemplateBackground";
import type { PageTemplate } from "./paperTemplates";

/** Ready replica handle used for create-time blob publish + commit. */
export type ReadyReplica = {
  kind: "ready";
  coordinator: SyncCoordinator;
  mintId: (kind: "operation" | "stroke" | "folder" | "text" | "notebook" | "page") => string;
  createNotebookRender: () => import("../model/types").RenderProfileRef;
  blobApi: BlobApi;
};

export function initiatingCreateStillValid(opts: {
  mounted: boolean;
  initiating: ReadyReplica;
  currentReplica: { kind: string } | ReadyReplica;
  initiatingSession: LibrarySession;
  currentSession: LibrarySession;
}): boolean {
  return (
    opts.mounted &&
    opts.currentReplica === opts.initiating &&
    opts.currentSession === opts.initiatingSession
  );
}

/**
 * Publish background (blank = no PUT), then run `commit` only while `stillValid` remains true.
 * `commit` must use the initiating replica — never a later `replicaRef` snapshot.
 */
export async function publishThenCreateNotebook(opts: {
  template: PageTemplate;
  blobApi: BlobApi;
  stillValid: () => boolean;
  commit: (background: BackgroundRef) => Promise<boolean>;
  isOnline?: OnlineProbe;
}): Promise<{ ok: boolean; notice: string | null }> {
  let background: BackgroundRef;
  try {
    ({ background } = await publishTemplateBackground(opts.template, opts.blobApi, opts.isOnline));
  } catch (e) {
    return {
      ok: false,
      notice: e instanceof TemplatePublishError ? e.message : "Couldn’t prepare the paper background. Nothing was created.",
    };
  }
  if (!opts.stillValid()) {
    return { ok: false, notice: "Couldn’t create a notebook on this device. Nothing was changed." };
  }
  const ok = await opts.commit(background);
  return { ok, notice: null };
}

/** Queue one structure op on a specific ready replica (not a React-captured closure). */
export async function commitStructureOn(
  ready: ReadyReplica,
  structure: StructureIntent,
): Promise<boolean> {
  let operationId: string;
  try {
    operationId = ready.mintId("operation");
  } catch {
    return false;
  }
  const notebook_id = structure.notebook_id;
  const page_id =
    structure.type === "create_notebook"
      ? structure.first_page.page_id
      : structure.type === "create_page"
        ? structure.page.page_id
        : structure.type === "delete_page"
          ? structure.page_id
          : "";
  const op = await ready.coordinator.commit({ kind: "structure", operation_id: operationId, notebook_id, page_id, structure });
  return op !== null;
}

/** Queue one folder op on a specific ready replica. */
export async function commitFolderOn(ready: ReadyReplica, folder: FolderIntent): Promise<boolean> {
  let operationId: string;
  try {
    operationId = ready.mintId("operation");
  } catch {
    return false;
  }
  const op = await ready.coordinator.commit({ kind: "folder", operation_id: operationId, notebook_id: "", page_id: "", folder });
  return op !== null;
}
