import { readFileSync } from "node:fs";
import { describe, expect, it, vi } from "vitest";
import { UnauthorizedError, type ReadOnlyApi } from "../src/api/readOnlyApi";
import { linkJumpLabel, MISSING_LINK_LABEL, preserveSelection, resolve, resolveLinkTarget, stepPage } from "../src/model/selection";
import type { LibraryHead, SnapshotResponse, SyncSnapshotV1 } from "../src/model/types";
import { validateSnapshot } from "../src/model/validate";
import { BlobCache } from "../src/rendering/background";
import { LibrarySession, POLL_MS, type Clock } from "../src/state/librarySession";
import { blobBytes, clone, manifest, snapshotJson } from "./fixtures";

const base = validateSnapshot(snapshotJson);
const nb = (s: SyncSnapshotV1, i: number) => s.notebooks[i]!;

describe("selection preservation across refreshed snapshots", () => {
  it("keeps the same notebook/page by identity even if moved", () => {
    const sel = { notebookId: nb(base, 0).notebook_id, pageId: nb(base, 0).pages[2]!.page_id };
    const next = clone(base);
    next.notebooks.reverse();
    next.notebooks[1]!.pages.reverse();
    expect(preserveSelection(base, sel, next)).toEqual(sel);
  });

  it("deleted page → the page now at its old index (clamped)", () => {
    const sel = { notebookId: nb(base, 0).notebook_id, pageId: nb(base, 0).pages[1]!.page_id };
    const next = clone(base);
    next.notebooks[0]!.pages.splice(1, 1);
    expect(preserveSelection(base, sel, next)).toEqual({ notebookId: sel.notebookId, pageId: next.notebooks[0]!.pages[1]!.page_id });
    const last = { notebookId: nb(base, 0).notebook_id, pageId: nb(base, 0).pages[2]!.page_id };
    const next2 = clone(base);
    next2.notebooks[0]!.pages.splice(2, 1);
    expect(preserveSelection(base, last, next2)!.pageId).toBe(next2.notebooks[0]!.pages[1]!.page_id);
  });

  it("deleted notebook → nearest surviving notebook, first page", () => {
    const sel = { notebookId: nb(base, 1).notebook_id, pageId: nb(base, 1).pages[1]!.page_id };
    const next = clone(base);
    next.notebooks.splice(1, 1);
    expect(preserveSelection(base, sel, next)).toEqual({ notebookId: next.notebooks[0]!.notebook_id, pageId: next.notebooks[0]!.pages[0]!.page_id });
  });

  it("no previous selection → first notebook/page; empty library → none", () => {
    expect(preserveSelection(null, null, base)).toEqual({ notebookId: nb(base, 0).notebook_id, pageId: nb(base, 0).pages[0]!.page_id });
    expect(preserveSelection(base, null, { ...base, notebooks: [] })).toBeNull();
  });

  it("paging stays within the notebook", () => {
    const sel = { notebookId: nb(base, 0).notebook_id, pageId: nb(base, 0).pages[0]!.page_id };
    expect(stepPage(base, sel, -1)).toBeNull();
    const n = stepPage(base, sel, 1)!;
    expect(resolve(base, n)!.pageIndex).toBe(1);
    expect(stepPage(base, { ...sel, pageId: nb(base, 0).pages[2]!.page_id }, 1)).toBeNull();
  });

  it("link jump uses the target notebook/page ids and fails closed when the dest is gone", () => {
    const src = nb(base, 0);
    const dst = nb(base, 1);
    const link = {
      link_id: "01a0c450-6c14-7401-8203-04050607081a",
      source_notebook_id: src.notebook_id,
      source_page_id: src.pages[0]!.page_id,
      source_x: 100,
      source_y: 200,
      target_notebook_id: dst.notebook_id,
      target_page_id: dst.pages[1]!.page_id,
    };
    expect(resolveLinkTarget(base, link)).toEqual({ notebookId: dst.notebook_id, pageId: dst.pages[1]!.page_id });
    expect(linkJumpLabel(base, link)).toBe(`${dst.title} - Page 2`);
    expect(resolveLinkTarget(base, { ...link, target_page_id: "01a0c450-6c14-7401-8203-000000000000" })).toBeNull();
    expect(linkJumpLabel(base, { ...link, target_page_id: "01a0c450-6c14-7401-8203-000000000000" })).toBe(MISSING_LINK_LABEL);
    expect(resolveLinkTarget(base, { ...link, target_notebook_id: "01a0c450-6c14-7401-8203-000000000000" })).toBeNull();
  });
});

// ------------------------------------------------------------------ polling session

function fakeClock() {
  let hidden = false;
  const timers: (() => void)[] = [];
  let onVis: (() => void) | null = null;
  const clock: Clock = {
    setTimeout: (fn, ms) => {
      expect(ms).toBe(POLL_MS);
      timers.push(fn);
      return timers.length;
    },
    clearTimeout: () => undefined,
    hidden: () => hidden,
    onVisibilityChange: (fn) => {
      onVis = fn;
      return () => (onVis = null);
    },
  };
  return {
    clock,
    async tick() {
      const fn = timers.shift();
      fn?.();
      await flush();
    },
    setHidden(h: boolean) {
      hidden = h;
      onVis?.();
    },
    pending: () => timers.length,
  };
}

async function flush() {
  for (let i = 0; i < 20; i++) await Promise.resolve();
}

function respond(s: SyncSnapshotV1, seq: number | bigint, sha: string): SnapshotResponse {
  return { library_id: s.library_id, through_server_seq: BigInt(seq), snapshot_sha256: sha, snapshot: s };
}

function fakeApi(initial: SnapshotResponse) {
  let cur = initial;
  const calls: string[] = [];
  let fail: Error | null = null;
  const api: ReadOnlyApi = {
    listLibraries: async () => [],
    libraryHead: async (): Promise<LibraryHead> => {
      calls.push("head");
      if (fail) throw fail;
      return { library_id: cur.library_id, genesis_sha256: manifest.snapshot_sha256, head_server_seq: cur.through_server_seq, snapshot_sha256: cur.snapshot_sha256 };
    },
    snapshot: async () => {
      calls.push("snapshot");
      if (fail) throw fail;
      return cur;
    },
    blob: async () => {
      calls.push("blob");
      return blobBytes;
    },
  };
  return {
    api,
    calls,
    set: (r: SnapshotResponse) => (cur = r),
    fail: (e: Error | null) => (fail = e),
  };
}

const SHA2 = "b".repeat(64);

describe("LibrarySession polling", () => {
  it("loads, skips unchanged heads, swaps coherently on change and keeps the selection", async () => {
    const f = fakeApi(respond(base, 0, manifest.snapshot_sha256));
    const c = fakeClock();
    const blobs = new BlobCache((sha) => f.api.blob(sha), async () => ({}) as CanvasImageSource);
    const s = new LibrarySession(f.api, base.library_id, blobs, () => undefined, c.clock);
    await s.start();
    expect(f.calls).toEqual(["snapshot"]);
    expect(s.state.current!.through_server_seq).toBe(0n);

    const sel = { notebookId: nb(base, 1).notebook_id, pageId: nb(base, 1).pages[0]!.page_id };
    s.select(sel);
    await vi.waitFor(() => expect(blobs.get(manifest.blob.sha256)).toBeDefined());

    f.calls.length = 0;
    await c.tick();
    expect(f.calls).toEqual(["head"]); // unchanged: no snapshot fetch

    const next = clone(base);
    next.notebooks[1]!.title = "Sketches (renamed)";
    f.set(respond(next, 5, SHA2));
    f.calls.length = 0;
    await c.tick();
    expect(f.calls).toEqual(["head", "snapshot"]); // blob cached: not refetched
    expect(s.state.current!.through_server_seq).toBe(5n);
    expect(s.state.selection).toEqual(sel);
    s.stop();
  });

  it("a failed refresh keeps the previous valid state and marks it stale", async () => {
    const f = fakeApi(respond(base, 0, manifest.snapshot_sha256));
    const c = fakeClock();
    const s = new LibrarySession(f.api, base.library_id, new BlobCache(f.api.blob, async () => ({}) as CanvasImageSource), () => undefined, c.clock);
    await s.start();
    const before = s.state.current;
    f.fail(new Error("server unreachable"));
    await c.tick();
    expect(s.state.current).toBe(before);
    expect(s.state.stale).toMatch(/unreachable/);
    f.fail(null);
    await c.tick();
    expect(s.state.stale).toBeNull();
    s.stop();
  });

  it("401 locks the app instead of retrying forever", async () => {
    const f = fakeApi(respond(base, 0, manifest.snapshot_sha256));
    const c = fakeClock();
    const onUnauth = vi.fn();
    const s = new LibrarySession(f.api, base.library_id, new BlobCache(f.api.blob, async () => ({}) as CanvasImageSource), onUnauth, c.clock);
    await s.start();
    f.fail(new UnauthorizedError());
    await c.tick();
    expect(onUnauth).toHaveBeenCalledTimes(1);
    s.stop();
    expect(c.pending()).toBeGreaterThanOrEqual(0);
  });

  it("pauses while hidden and polls immediately when visible again", async () => {
    const f = fakeApi(respond(base, 0, manifest.snapshot_sha256));
    const c = fakeClock();
    const s = new LibrarySession(f.api, base.library_id, new BlobCache(f.api.blob, async () => ({}) as CanvasImageSource), () => undefined, c.clock);
    await s.start();
    c.setHidden(true);
    await flush();
    f.calls.length = 0;
    await c.tick();
    await c.tick();
    expect(f.calls).toEqual([]); // hidden: no requests
    c.setHidden(false);
    await flush();
    expect(f.calls).toEqual(["head"]);
    s.stop();
  });
});

describe("LibrarySession notebook-open observation (replica-local recency)", () => {
  async function started() {
    const f = fakeApi(respond(base, 0, manifest.snapshot_sha256));
    const c = fakeClock();
    const opened: string[] = [];
    const s = new LibrarySession(
      f.api,
      base.library_id,
      new BlobCache(f.api.blob, async () => ({}) as CanvasImageSource),
      () => undefined,
      c.clock,
      (id) => opened.push(id),
    );
    await s.start();
    return { f, c, s, opened };
  }
  const first = (i: number) => ({ notebookId: nb(base, i).notebook_id, pageId: nb(base, i).pages[0]!.page_id });

  it("initial load, restored selection and snapshot refreshes never count as an open", async () => {
    const { f, c, s, opened } = await started();
    expect(s.state.selection).toEqual(first(0)); // auto-selected by the load
    const next = clone(base);
    next.notebooks[1]!.title = "Renamed elsewhere";
    f.set(respond(next, 7, SHA2));
    await c.tick();
    expect(s.state.current!.through_server_seq).toBe(7n);
    expect(opened).toEqual([]);
    s.stop();
  });

  it("switching notebooks records an open each time; page steps and re-selecting the open notebook do not", async () => {
    const { s, opened } = await started();
    s.select(first(1));
    expect(opened).toEqual([nb(base, 1).notebook_id]);
    const n1 = nb(base, 1);
    if (n1.pages.length > 1) s.select({ notebookId: n1.notebook_id, pageId: n1.pages[1]!.page_id });
    s.select(first(1));
    expect(opened).toEqual([n1.notebook_id]);
    s.select(first(0));
    s.select(first(1));
    expect(opened).toEqual([n1.notebook_id, nb(base, 0).notebook_id, n1.notebook_id]);
    s.stop();
  });

  it("selection only changes through select() or refresh(), and the UI navigates only via select()", () => {
    const sessionSrc = readFileSync(new URL("../src/state/librarySession.ts", import.meta.url), "utf8");
    expect(sessionSrc.match(/this\.set\(\{[^}]*selection/g)).toHaveLength(2);
    expect(sessionSrc).toMatch(/select\(sel: Selection\): void \{\s*const opened[^\n]*\n\s*this\.set\(\{ selection: sel \}\);\s*if \(opened\) this\.onNotebookOpened/);
    // State stays independent of UI preferences/IndexedDB; the app shell injects the recorder.
    expect(sessionSrc).not.toMatch(/libraryPrefs|prefsDb|indexedDB/);
    const app = readFileSync(new URL("../src/app/App.tsx", import.meta.url), "utf8");
    expect(app.match(/new LibrarySession\(/g)).toHaveLength(1);
    expect(app).toMatch(/new LibrarySession\([^;]*recordNotebookAccess\)/);
    expect(app).toMatch(/replicaUnavailableReason/);
    expect(app).toMatch(/startWritableReplica/);
    expect(app).not.toMatch(/Promise\.all\(\[loadBridge\(\),\s*ReplicaStore\.open\(\)\]\)/);
    expect(app).toMatch(/await coordinator\.idle\(\)/);
    for (const f of ["components/Viewer.tsx", "components/NotebookSidebar.tsx", "components/PageView.tsx", "app/App.tsx"]) {
      const text = readFileSync(new URL(`../src/${f}`, import.meta.url), "utf8");
      expect(text, f).not.toMatch(/state\.selection\s*=|\.set\(\{\s*selection/);
    }
  });
});
