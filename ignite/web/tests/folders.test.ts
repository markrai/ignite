// Folders in ignite-web: strict snapshot validation, envelopes built only by the Rust bridge,
// durable queueing through the one sync path, and the reducer-backed pending projection.

import { describe, expect, it } from "vitest";
import { ProtocolViolation, validateSnapshot } from "../src/model/validate";
import { SyncCoordinator } from "../src/replica/coordinator";
import { ReplicaStore } from "../src/replica/store";
import { clone, snapshotJson } from "./fixtures";
import { bridge, encoderFor, FakeServer, freshIdb, LIB } from "./replicaHelpers";

type J = Record<string, any>; // eslint-disable-line @typescript-eslint/no-explicit-any

const F1 = "01a0c450-6c14-7401-8203-040506070814";
const F2 = "01a0c450-6c15-7401-8203-040506070815";

function withFolders(): J {
  const s = clone(snapshotJson) as J;
  s.folders = [
    { folder_id: F1, title: "Journals" },
    { folder_id: F2, title: "Empty" },
  ];
  s.notebooks[1].folder_id = F1;
  return s;
}

describe("snapshot folders", () => {
  it("a snapshot without folders validates to an empty folder list", () => {
    const s = validateSnapshot(snapshotJson);
    expect(s.folders).toEqual([]);
    expect(s.notebooks.every((n) => n.folder_id === undefined)).toBe(true);
  });

  it("keeps folders in order and memberships", () => {
    const s = validateSnapshot(withFolders());
    expect(s.folders.map((f) => f.title)).toEqual(["Journals", "Empty"]);
    expect(s.notebooks[1]!.folder_id).toBe(F1);
    expect(s.notebooks[0]!.folder_id).toBeUndefined();
  });

  it.each([
    ["unknown membership", (s: J) => (s.notebooks[0].folder_id = "01a0c450-6c16-7401-8203-040506070816")],
    ["duplicate folder", (s: J) => s.folders.push({ folder_id: F1, title: "Again" })],
    ["empty folder title", (s: J) => (s.folders[0].title = "  ")],
    ["unknown folder field", (s: J) => (s.folders[0].color = "red")],
    ["bad folder id", (s: J) => (s.folders[1].folder_id = "nope")],
  ])("refuses %s", (_name, mutate) => {
    const s = withFolders();
    mutate(s);
    expect(() => validateSnapshot(s)).toThrow(ProtocolViolation);
  });
});

describe("folder operations", () => {
  it("are encoded by the bridge, queued durably, and sent as exact bytes", async () => {
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
    const folderId = b.mintId("folder");
    const op = await c.commit({
      kind: "folder",
      operation_id: b.mintId("operation"),
      notebook_id: "",
      page_id: "",
      folder: { type: "create_folder", folder_id: folderId, title: "Journals" },
    });
    expect(op).not.toBeNull();
    const env = JSON.parse(op!.envelope) as J;
    expect(env.operation).toEqual({ type: "create_folder", folder_id: folderId, title: "Journals" });
    expect(c.state.overlay.at(-1)).toMatchObject({ kind: "folder", folder: { type: "create_folder" } });
    await c.idle();
    expect(server.bodies.join("")).toContain(op!.envelope);

    // Moving back to the top level omits folder_id on the wire (never null).
    const back = await c.commit({
      kind: "folder",
      operation_id: b.mintId("operation"),
      notebook_id: "",
      page_id: "",
      folder: { type: "set_notebook_folder", notebook_id: "01a0c450-6c04-7401-8203-040506070804" },
    });
    expect(back!.envelope).not.toContain("folder_id");
    expect(back!.envelope).not.toContain("null");
    await c.idle();
    c.stop();
  });

  it("an invalid folder title is refused by Rust and nothing is queued", async () => {
    const b = await bridge();
    const store = await ReplicaStore.open(freshIdb());
    const c = new SyncCoordinator({
      store,
      api: new FakeServer(),
      libraryId: LIB,
      mintReplicaId: () => b.mintId("replica"),
      encoder: await encoderFor(),
      requestRefresh: () => undefined,
      onUnauthorized: () => undefined,
    });
    await c.start();
    const op = await c.commit({
      kind: "folder",
      operation_id: b.mintId("operation"),
      notebook_id: "",
      page_id: "",
      folder: { type: "rename_folder", folder_id: F1, title: " " },
    });
    expect(op).toBeNull();
    expect(c.state.pending).toBe(0);
    expect(c.state.saveError).toContain("folder");
    c.stop();
  });

  it("pending folder ops project by the reducer's rules", async () => {
    const b = await bridge();
    const s = validateSnapshot(withFolders());
    const members = s.notebooks.map((n) => (n.folder_id ? { notebook_id: n.notebook_id, folder_id: n.folder_id } : { notebook_id: n.notebook_id }));
    const nb0 = s.notebooks[0]!.notebook_id;
    const nb1 = s.notebooks[1]!.notebook_id;
    const out = b.projectFolders(s.folders, members, [
      { type: "set_notebook_folder", notebook_id: nb0, folder_id: F2 },
      { type: "rename_folder", folder_id: F2, title: "Work" },
      { type: "delete_folder", folder_id: F1 },
      // Refused by the reducer (never existed): skipped, never an error.
      { type: "delete_folder", folder_id: "01a0c450-6c16-7401-8203-040506070816" },
    ]);
    expect(out.changed).toEqual([true, true, true, false]);
    expect(out.folders).toEqual([{ folder_id: F2, title: "Work" }]);
    expect(out.notebooks).toEqual([
      { notebook_id: nb0, folder_id: F2 },
      { notebook_id: nb1 },
    ]);
  });
});
