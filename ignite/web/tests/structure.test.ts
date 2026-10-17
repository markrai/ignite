// Notebook/page structure + undo: WASM-encoded envelopes, durable queue, unified project_library.

import { describe, expect, it } from "vitest";
import { orderLibrary } from "../src/model/librarySort";
import { preserveSelection, resolve } from "../src/model/selection";
import { validateSnapshot } from "../src/model/validate";
import { SyncCoordinator } from "../src/replica/coordinator";
import { ReplicaStore } from "../src/replica/store";
import { clone, snapshotJson } from "./fixtures";
import { bridge, encoderFor, FakeServer, freshIdb, LIB, NB, PG } from "./replicaHelpers";

type J = Record<string, unknown>;

describe("structure operations", () => {
  it("create notebook draft carries first_page and is sent as exact bytes", async () => {
    const b = await bridge();
    const store = await ReplicaStore.open(freshIdb());
    const server = new FakeServer();
    const c = new SyncCoordinator({
      store,
      api: server,
      libraryId: LIB,
      mintReplicaId: () => b.mintId("replica"),
      encoder: await encoderFor(),
      requestRefresh: () => undefined,
      onUnauthorized: () => undefined,
    });
    await c.start();
    const notebookId = b.mintId("notebook");
    const pageId = b.mintId("page");
    const render = b.createNotebookRender();
    expect(render.rasterizer_id).toBe("scribe-raster-v2-pressure");
    expect(render.ink_byte).toBe(0);
    const op = await c.commit({
      kind: "structure",
      operation_id: b.mintId("operation"),
      notebook_id: notebookId,
      page_id: pageId,
      structure: {
        type: "create_notebook",
        notebook_id: notebookId,
        title: "Lab book",
        after_notebook_id: NB,
        first_page: { page_id: pageId, background: { kind: "blank" }, render },
      },
    });
    expect(op).not.toBeNull();
    const env = JSON.parse(op!.envelope) as J;
    const operation = env.operation as J;
    expect(operation.type).toBe("create_notebook");
    expect(operation.notebook_id).toBe(notebookId);
    expect(operation.first_page).toEqual({ page_id: pageId, background: { kind: "blank" }, render });
    expect(c.state.overlay.at(-1)).toMatchObject({ kind: "structure", structure: { type: "create_notebook" } });
    await c.idle();
    expect(server.bodies.join("")).toContain(op!.envelope);
    c.stop();
  });

  it("pending CreateNotebook then SetNotebookFolder files the new notebook; a ghost folder leaves it unfiled", async () => {
    const b = await bridge();
    const s = validateSnapshot(clone(snapshotJson));
    const last = s.notebooks.at(-1)!.notebook_id;
    const notebookId = b.mintId("notebook");
    const pageId = b.mintId("page");
    const folderId = b.mintId("folder");
    const render = b.createNotebookRender();
    const create = {
      type: "create_notebook" as const,
      notebook_id: notebookId,
      title: "In folder",
      after_notebook_id: last,
      first_page: { page_id: pageId, background: { kind: "blank" as const }, render },
    };
    const filed = b.projectLibrary(s, [
      { type: "create_folder", folder_id: folderId, title: "Work" },
      create,
      { type: "set_notebook_folder", notebook_id: notebookId, folder_id: folderId },
    ]);
    expect(filed.changed).toEqual([true, true, true]);
    expect(filed.notebooks.find((n) => n.notebook_id === notebookId)?.folder_id).toBe(folderId);

    const ghost = "01a0c450-6c16-7401-8203-040506070816";
    const orphan = b.projectLibrary(s, [create, { type: "set_notebook_folder", notebook_id: notebookId, folder_id: ghost }]);
    expect(orphan.changed).toEqual([true, false]);
    expect(orphan.notebooks.find((n) => n.notebook_id === notebookId)?.folder_id).toBeUndefined();
    expect(orphan.notebooks).toHaveLength(s.notebooks.length + 1);
  });

  it("CreateNotebook projection fills omitted texts/links so the first view pass can read .length", async () => {
    const b = await bridge();
    const s = validateSnapshot(clone(snapshotJson));
    const last = s.notebooks.at(-1)!.notebook_id;
    const notebookId = b.mintId("notebook");
    const pageId = b.mintId("page");
    const render = b.createNotebookRender();
    const create = {
      type: "create_notebook" as const,
      notebook_id: notebookId,
      title: "Journal",
      after_notebook_id: last,
      first_page: { page_id: pageId, background: { kind: "blank" as const }, render },
    };

    // Wire omit: empty texts/links are absent on the WASM JSON (the Chrome crash receiver).
    const raw = JSON.parse(b.call({ fn: "project_library", snapshot: s, ops: [create] }).ok!) as {
      notebooks: { notebook_id: string; pages: { links?: unknown; texts?: unknown }[] }[];
      folders?: unknown;
    };
    const rawNb = raw.notebooks.find((n) => n.notebook_id === notebookId)!;
    expect(rawNb.pages[0]!.links).toBeUndefined();
    expect(rawNb.pages[0]!.texts).toBeUndefined();

    const out = b.projectLibrary(s, [create]);
    const created = out.notebooks.find((n) => n.notebook_id === notebookId)!;
    for (const n of out.notebooks) {
      for (const p of n.pages) {
        expect(Array.isArray(p.links)).toBe(true);
        expect(Array.isArray(p.texts)).toBe(true);
      }
    }
    expect(Array.isArray(out.folders)).toBe(true);
    expect(created.pages[0]!.links).toEqual([]);
    expect(created.pages[0]!.texts).toEqual([]);

    // Viewer: merge projection, select the new page, sidebar pages() + PageView links.length.
    const view = { ...s, notebooks: out.notebooks, folders: out.folders };
    const r = resolve(view, { notebookId, pageId });
    expect(r).not.toBeNull();
    expect(r!.page.links.length).toBe(0);
    expect(r!.notebook.pages.length).toBe(1);
    expect(`${r!.notebook.pages.length} ${r!.notebook.pages.length === 1 ? "page" : "pages"}`).toBe("1 page");
    const membership = new Map<string, string>();
    for (const n of view.notebooks) if (n.folder_id) membership.set(n.notebook_id, n.folder_id);
    expect(() => orderLibrary(view.notebooks, view.folders, membership, "recent", new Map())).not.toThrow();
  });

  it("projected sole notebook/page counts include pending creates and deletes", async () => {
    const b = await bridge();
    const s = validateSnapshot(clone(snapshotJson));
    expect(s.notebooks.length).toBeGreaterThan(1);
    const deleted = b.projectLibrary(s, [{ type: "delete_notebook", notebook_id: s.notebooks[1]!.notebook_id }]);
    expect(deleted.notebooks).toHaveLength(s.notebooks.length - 1);
    expect(deleted.notebooks.length === 1).toBe(s.notebooks.length === 2);

    const nb = s.notebooks[0]!;
    const pageId = b.mintId("page");
    const inserted = b.projectLibrary(s, [
      {
        type: "create_page",
        notebook_id: nb.notebook_id,
        after_page_id: nb.pages[0]!.page_id,
        page: { page_id: pageId, background: nb.pages[0]!.background, render: nb.pages[0]!.render },
      },
    ]);
    expect(inserted.notebooks[0]!.pages).toHaveLength(nb.pages.length + 1);
    expect(inserted.notebooks[0]!.pages[1]!.page_id).toBe(pageId);

    const afterCreate = { ...s, notebooks: inserted.notebooks, folders: inserted.folders };
    const keep = preserveSelection(s, { notebookId: nb.notebook_id, pageId: nb.pages[0]!.page_id }, afterCreate);
    expect(keep).toEqual({ notebookId: nb.notebook_id, pageId: nb.pages[0]!.page_id });
    const afterDelete = b.projectLibrary(s, [{ type: "delete_page", notebook_id: nb.notebook_id, page_id: nb.pages[0]!.page_id }]);
    const remapped = preserveSelection(
      s,
      { notebookId: nb.notebook_id, pageId: nb.pages[0]!.page_id },
      { ...s, notebooks: afterDelete.notebooks, folders: afterDelete.folders },
    );
    expect(remapped).toEqual({ notebookId: nb.notebook_id, pageId: afterDelete.notebooks[0]!.pages[0]!.page_id });
  });
});

describe("undo", () => {
  it("queues DeleteStrokeLineage of the lineage root", async () => {
    const b = await bridge();
    const store = await ReplicaStore.open(freshIdb());
    const server = new FakeServer();
    const c = new SyncCoordinator({
      store,
      api: server,
      libraryId: LIB,
      mintReplicaId: () => b.mintId("replica"),
      encoder: await encoderFor(),
      requestRefresh: () => undefined,
      onUnauthorized: () => undefined,
    });
    await c.start();
    const root = b.mintId("stroke");
    const op = await c.commit({
      kind: "undo",
      operation_id: b.mintId("operation"),
      notebook_id: NB,
      page_id: PG,
      root_id: root,
    });
    expect(op).not.toBeNull();
    const env = JSON.parse(op!.envelope) as { operation: { type: string; root_id: string } };
    expect(env.operation).toEqual({ type: "delete_stroke_lineage", notebook_id: NB, page_id: PG, root_id: root });
    await c.idle();
    c.stop();
  });

  it("project_page applies pending add then delete_lineage", async () => {
    const b = await bridge();
    const sid = b.mintId("stroke");
    const stroke = { stroke_id: sid, root_id: sid, style: "legacy-fixed-v1" as const, points: [{ x: 100, y: 200 }, { x: 300, y: 400 }] };
    const out = b.projectPage([], [
      { op: "add", stroke },
      { op: "delete_lineage", root_id: sid },
    ]);
    expect(out.changed).toEqual([true, true]);
    expect(out.strokes).toEqual([]);
  });
});
