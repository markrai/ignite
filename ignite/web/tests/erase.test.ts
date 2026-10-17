import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { EraseCapture, MAX_STROKE_POINTS } from "../src/authoring/capture";
import { ERASER_RADIUS } from "../src/model/generated/protocolConstants";
import type { SyncStroke } from "../src/model/types";
import { SyncCoordinator } from "../src/replica/coordinator";
import { toSeq20 } from "../src/replica/seq";
import { DB_NAME, ReplicaStore } from "../src/replica/store";
import type { ScreenTransform } from "../src/rendering/viewport";
import { bridge, draft, encoderFor, eraseDraft, FakeServer, freshIdb, LIB, NB, PG } from "./replicaHelpers";

const PAGE = { width: 15624, height: 20832 };
const T: ScreenTransform = { scale: 1, offsetX: 0, offsetY: 0 };
const s = (x: number, y: number) => ({ localX: x, localY: y, pressure: 0 });

describe("erase capture", () => {
  it("must start on the page; later samples clamp; no pressure; exact repeats dropped", () => {
    expect(EraseCapture.begin(T, PAGE, s(-1, 5))).toBeNull();
    const c = EraseCapture.begin(T, PAGE, s(10.1, 20))!;
    c.add(T, s(10.1, 20));
    c.add(T, s(-50, 30000));
    c.add(T, s(99999, -1));
    expect(c.path).toEqual([
      { x: Math.fround(10.1), y: 20 },
      { x: 0, y: 20832 },
      { x: 15624, y: 0 },
    ]);
  });
  it("refuses an oversize sweep", () => {
    const c = EraseCapture.begin(T, PAGE, s(0, 0))!;
    for (let i = 1; i <= MAX_STROKE_POINTS + 3; i++) c.add(T, s(i % 15000, Math.floor(i / 15000)));
    expect(c.tooLong).toBe(true);
  });
  it("the eraser radius is the Rust-generated Kindle value", () => {
    expect(ERASER_RADIUS).toBe(240);
  });
});

interface Case {
  name: string;
  request: Record<string, unknown>;
  ok?: string;
}
const fixture = JSON.parse(readFileSync(new URL("./fixtures/wasm-envelopes-v1.json", import.meta.url), "utf8")) as { cases: Case[] };

describe("typed bridge wrappers reproduce the Rust fixture", () => {
  it("encodeEraseRegion (radius from Rust, roots sorted/unique)", async () => {
    const b = await bridge();
    const cases = fixture.cases.filter((c) => c.request.fn === "encode_erase_region" && c.ok !== undefined);
    expect(cases.length).toBeGreaterThanOrEqual(5);
    for (const c of cases) {
      const rest = Object.fromEntries(Object.entries(c.request).filter(([k]) => k !== "fn"));
      const out = b.encodeEraseRegion(rest as never);
      expect(out).toBe(c.ok);
      const env = JSON.parse(out) as { operation: { radius: number; observed_roots: string[] } };
      expect(env.operation.radius).toBe(ERASER_RADIUS);
      expect([...env.operation.observed_roots].sort()).toEqual(env.operation.observed_roots);
      expect(new Set(env.operation.observed_roots).size).toBe(env.operation.observed_roots.length);
    }
  });

  it("projectPage equals the reducer's result for every pinned scenario", async () => {
    const b = await bridge();
    const cases = fixture.cases.filter((c) => c.request.fn === "project_page" && c.ok !== undefined);
    expect(cases.length).toBeGreaterThanOrEqual(7);
    for (const c of cases) {
      const got = b.projectPage(c.request.strokes as SyncStroke[], c.request.ops as never);
      expect(got, c.name).toEqual(JSON.parse(c.ok!));
    }
  });
});

describe("erase through the replica store and coordinator", () => {
  it("an erase commits one EraseRegion envelope with the next client_seq, and syncs", async () => {
    const store = await ReplicaStore.open(freshIdb());
    const b = await bridge();
    const server = new FakeServer();
    const c = new SyncCoordinator({
      store,
      api: server,
      libraryId: LIB,
      mintReplicaId: () => b.mintId("replica"),
      encoder: await encoderFor(),
      requestRefresh: () => undefined,
      onUnauthorized: () => undefined,
      setTimeout: () => 0,
      clearTimeout: () => undefined,
      random: () => 0.5,
    });
    await c.start();
    const add = await draft();
    await c.commit(add);
    const er = await eraseDraft([add.stroke.root_id, add.stroke.root_id], 200);
    const op = await c.commit(er);
    expect(op).not.toBeNull();
    expect(op!.client_seq).toBe(toSeq20(2n));
    const env = JSON.parse(op!.envelope) as { client_seq: number; operation: { type: string; observed_roots: string[]; radius: number; path: unknown[] } };
    expect(env.client_seq).toBe(2);
    expect(env.operation.type).toBe("erase_region");
    expect(env.operation.observed_roots).toEqual([add.stroke.root_id]);
    expect(env.operation.radius).toBe(240);
    // The overlay carries the erase intent in client_seq order after the add.
    expect(c.state.overlay.map((o) => o.kind)).toEqual(["add", "erase"]);
    await c.idle();
    expect(server.head).toBe(2n);
    expect(server.bodies.join("")).toContain('"type":"erase_region"');
    expect(c.state.pending).toBe(0);
    const acc = await store.accepted(LIB);
    expect(acc.map((a) => a.kind ?? "add")).toEqual(["add", "erase"]);
  });

  it("a refused erase encode consumes no sequence", async () => {
    const store = await ReplicaStore.open(freshIdb());
    const b = await bridge();
    await store.ensureReplica(LIB, () => b.mintId("replica"));
    const bad = await eraseDraft([]); // no observed roots: the protocol refuses it
    await expect(store.commitOp(LIB, bad, await encoderFor())).rejects.toThrow();
    expect((await store.replica(LIB))!.last_client_seq).toBe(toSeq20(0n));
  });

  it("rows written before erase existed (no `kind`) still read as AddStroke", async () => {
    const idb = freshIdb();
    const store = await ReplicaStore.open(idb);
    const b = await bridge();
    await store.ensureReplica(LIB, () => b.mintId("replica"));
    const d = await draft();
    const op = await store.commitOp(LIB, d, await encoderFor());
    expect("kind" in op).toBe(false); // the S3B row shape
    const c = new SyncCoordinator({
      store,
      api: new FakeServer(),
      libraryId: LIB,
      mintReplicaId: () => b.mintId("replica"),
      encoder: await encoderFor(),
      requestRefresh: () => undefined,
      onUnauthorized: () => undefined,
      setTimeout: () => 0,
      clearTimeout: () => undefined,
      random: () => 0.5,
      locks: null,
    });
    await c.start();
    await c.idle();
    expect(c.state.overlay[0]).toMatchObject({ kind: "add", notebook_id: NB, page_id: PG });
    expect(DB_NAME).toBe("scribe-web-sync");
  });
});
