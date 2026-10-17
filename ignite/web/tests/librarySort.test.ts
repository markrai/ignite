// Home/library sorting: presentation-only orders derived from the canonical arrays, and the
// replica-local sort mode + notebook-open recency kept in `scribe-web-prefs`.

import "fake-indexeddb/auto";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { ACCESS_KEY, LibraryPrefs, parseAccess, parseSortMode, SORT_KEY } from "../src/app/libraryPrefs";
import { prefsStore, type PrefsStore } from "../src/app/prefsDb";
import { orderLibrary, SORT_LABELS, SORT_MODES, type SortMode } from "../src/model/librarySort";
import type { SnapshotFolder, SnapshotNotebook } from "../src/model/types";
import { freshIdb } from "./replicaHelpers";

const nb = (id: string, title: string, folder_id?: string): SnapshotNotebook => ({ notebook_id: id, title, pages: [], ...(folder_id ? { folder_id } : {}) });
const fo = (id: string, title: string): SnapshotFolder => ({ folder_id: id, title });

// Canonical order: folders F-zeta, F-alpha, F-mid; notebooks interleaved across folders/top.
const folders = [fo("fz", "zeta"), fo("fa", "Alpha"), fo("fm", "mid")];
const notebooks = [
  nb("n1", "delta", "fz"),
  nb("n2", "Bravo"),
  nb("n3", "alpha", "fz"),
  nb("n4", "charlie"),
  nb("n5", "Echo", "fa"),
  nb("n6", "apple"),
  nb("n7", "beta", "fz"),
];
const membership = new Map(notebooks.flatMap((n) => (n.folder_id ? [[n.notebook_id, n.folder_id] as const] : [])));

function view(mode: SortMode, access: ReadonlyMap<string, number> = new Map(), nbs = notebooks, fs = folders) {
  const o = orderLibrary(nbs, fs, membership, mode, access);
  return {
    folders: o.folders.map((f) => f.folder_id),
    byFolder: Object.fromEntries([...o.byFolder].map(([k, v]) => [k, v.map((n) => n.notebook_id)])),
    top: o.top.map((n) => n.notebook_id),
  };
}

function memStore(): PrefsStore & { data: Map<string, unknown>; puts: number } {
  const data = new Map<string, unknown>();
  const s = {
    data,
    puts: 0,
    get: async (k: string) => structuredClone(data.get(k)),
    put: async (k: string, v: unknown) => {
      s.puts++;
      data.set(k, structuredClone(v));
    },
  };
  return s;
}

describe("library order (presentation only)", () => {
  it("creation order is the canonical arrays: folder order and each bucket's canonical subsequence", () => {
    const v = view("creation");
    expect(v.folders).toEqual(["fz", "fa", "fm"]);
    expect(v.byFolder).toEqual({ fz: ["n1", "n3", "n7"], fa: ["n5"], fm: [] });
    expect(v.top).toEqual(["n2", "n4", "n6"]);
    // The canonical folders array is passed through, not reconstructed.
    expect(orderLibrary(notebooks, folders, membership, "creation", new Map()).folders).toBe(folders);
  });

  it("alphabetical sorts folders, notebooks within folders, and top-level notebooks (case-insensitive)", () => {
    const v = view("alphabetical");
    expect(v.folders).toEqual(["fa", "fm", "fz"]);
    expect(v.byFolder).toEqual({ fz: ["n3", "n7", "n1"], fa: ["n5"], fm: [] });
    expect(v.top).toEqual(["n6", "n2", "n4"]);
  });

  it("alphabetical ties fall back to canonical order", () => {
    const nbs = [nb("x1", "Same"), nb("x2", "same"), nb("x3", "SAME"), nb("x0", "Alpha")];
    expect(view("alphabetical", new Map(), nbs, []).top).toEqual(["x0", "x1", "x2", "x3"]);
    const fs = [fo("b", "Dup"), fo("a", "dup")];
    expect(view("alphabetical", new Map(), [], fs).folders).toEqual(["b", "a"]);
  });

  it("recently accessed: opened newest-first, then unopened in canonical order, per bucket", () => {
    const access = new Map([
      ["n7", 3],
      ["n3", 9],
      ["n6", 5],
      ["n4", 2],
    ]);
    const v = view("recent", access);
    expect(v.byFolder.fz).toEqual(["n3", "n7", "n1"]);
    expect(v.top).toEqual(["n6", "n4", "n2"]);
    // Folder order stays canonical even though fz holds the most recent notebook.
    expect(v.folders).toEqual(["fz", "fa", "fm"]);
  });

  it("recency never moves a notebook between folders", () => {
    const access = new Map(notebooks.map((n, i) => [n.notebook_id, 100 - i]));
    for (const mode of SORT_MODES) {
      const o = orderLibrary(notebooks, folders, membership, mode, access);
      for (const [fid, list] of o.byFolder) for (const n of list) expect(membership.get(n.notebook_id)).toBe(fid);
      for (const n of o.top) expect(membership.has(n.notebook_id)).toBe(false);
      expect([...o.byFolder.values()].flat().length + o.top.length).toBe(notebooks.length);
    }
  });

  it("stale access records for deleted notebooks are ignored", () => {
    const access = new Map([
      ["gone-1", 50],
      ["n4", 4],
      ["gone-2", 60],
    ]);
    expect(view("recent", access).top).toEqual(["n4", "n2", "n6"]);
  });

  it("never mutates the canonical arrays", () => {
    const nbs = structuredClone(notebooks);
    const fs = structuredClone(folders);
    const before = JSON.stringify([nbs, fs]);
    for (const mode of SORT_MODES) orderLibrary(nbs, fs, membership, mode, new Map([["n2", 1]]));
    expect(JSON.stringify([nbs, fs])).toBe(before);
  });
});

describe("library prefs (scribe-web-prefs)", () => {
  it("defaults to Creation Order with no history; malformed values read as defaults", async () => {
    const p = new LibraryPrefs(memStore());
    await p.settled();
    expect(p.state.sortMode).toBe("creation");
    expect(p.state.access.size).toBe(0);
    expect(parseSortMode(undefined)).toBe("creation");
    expect(parseSortMode("newest")).toBe("creation");
    expect(parseAccess(undefined).access.size).toBe(0);
    const a = parseAccess({ next: "x", byId: { a: 3, b: -1, c: 1.5, d: "7", e: 2 } });
    expect([...a.access]).toEqual([
      ["a", 3],
      ["e", 2],
    ]);
    expect(a.next).toBe(3);
  });

  it("cycles Creation → Alphabetical → Recently Accessed → Creation with the toast labels", () => {
    const p = new LibraryPrefs(memStore());
    const seen = [p.state.sortMode];
    for (let i = 0; i < 3; i++) seen.push(p.cycleSortMode());
    expect(seen).toEqual(["creation", "alphabetical", "recent", "creation"]);
    expect(seen.slice(1).map((m) => SORT_LABELS[m])).toEqual(["Alphabetical Order", "Recently Accessed", "Creation Order"]);
  });

  it("sort mode and access history survive a reload of the real IndexedDB store", async () => {
    const idb = freshIdb();
    const p = new LibraryPrefs(prefsStore(idb));
    p.cycleSortMode();
    p.cycleSortMode();
    p.recordAccess("A");
    p.recordAccess("B");
    p.recordAccess("A");
    await p.settled();

    const q = new LibraryPrefs(prefsStore(idb));
    await q.settled();
    expect(q.state.sortMode).toBe("recent");
    expect(q.state.access.get("A")).toBeGreaterThan(q.state.access.get("B")!);
    q.recordAccess("B");
    expect(q.state.access.get("B")).toBeGreaterThan(q.state.access.get("A")!);
    // Theme shares the database untouched.
    expect(await prefsStore(idb).get("theme")).toBeUndefined();
  });

  it("A, then B, then A again: each open moves that notebook ahead", () => {
    const p = new LibraryPrefs(memStore());
    const top = () => view("recent", p.state.access, [nb("A", "a"), nb("B", "b"), nb("C", "c")], []).top;
    p.recordAccess("A");
    expect(top()).toEqual(["A", "B", "C"]);
    p.recordAccess("B");
    expect(top()).toEqual(["B", "A", "C"]);
    p.recordAccess("A");
    expect(top()).toEqual(["A", "B", "C"]);
  });

  it("restoring stored prefs never overrides a choice made during the load, and early opens stay newest", async () => {
    const store = memStore();
    store.data.set(SORT_KEY, "alphabetical");
    store.data.set(ACCESS_KEY, { next: 40, byId: { old: 40 } });
    const p = new LibraryPrefs(store);
    p.recordAccess("early"); // before the load completes
    await p.settled();
    expect(p.state.sortMode).toBe("alphabetical");
    expect(p.state.access.get("early")).toBe(41);
    expect(store.data.get(ACCESS_KEY)).toEqual({ next: 41, byId: { old: 40, early: 41 } });

    const r = new LibraryPrefs(memStore());
    r.cycleSortMode();
    await r.settled();
    expect(r.state.sortMode).toBe("alphabetical");
  });

  it("storage failures are harmless", async () => {
    const broken: PrefsStore = { get: () => Promise.reject(new Error("blocked")), put: () => Promise.reject(new Error("blocked")) };
    const p = new LibraryPrefs(broken);
    p.cycleSortMode();
    p.recordAccess("A");
    await p.settled();
    expect(p.state.sortMode).toBe("alphabetical");
    expect(p.state.access.get("A")).toBe(1);
  });

  it("the sort/access path has no route to the sync coordinator or replica store", () => {
    for (const f of ["src/app/libraryPrefs.ts", "src/model/librarySort.ts", "src/app/prefsDb.ts"]) {
      const text = readFileSync(new URL(`../${f}`, import.meta.url), "utf8");
      expect(text, f).not.toMatch(/coordinator|replica\/store|replicaApi|scribe-web-sync|commit\(/);
    }
  });
});
