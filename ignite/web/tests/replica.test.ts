import { describe, expect, it } from "vitest";
import { SyncCoordinator, backoffDelay, conflictText, BACKOFF_MAX_MS } from "../src/replica/coordinator";
import { REPLICA_SEQUENCE_EXHAUSTED, toSeq20, U64_MAX } from "../src/replica/seq";
import { DB_NAME, ReplicaStore, StoreError } from "../src/replica/store";
import { bridge, draft, encoderFor, FakeServer, freshIdb, LIB } from "./replicaHelpers";

const S1 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

async function setup() {
  const idb = freshIdb();
  const store = await ReplicaStore.open(idb);
  const b = await bridge();
  let minted = 0;
  const mint = () => {
    minted++;
    return b.mintId("replica");
  };
  return { idb, store, b, mint, minted: () => minted, enc: await encoderFor() };
}

/** Every value in every store, as text (for "the token is never stored" checks). */
async function dumpAll(idb: IDBFactory): Promise<string> {
  const db = await new Promise<IDBDatabase>((res, rej) => {
    const r = idb.open(DB_NAME);
    r.onsuccess = () => res(r.result);
    r.onerror = () => rej(r.error);
  });
  const out: unknown[] = [];
  for (const name of Array.from(db.objectStoreNames)) {
    const all = await new Promise<unknown[]>((res) => {
      const r = db.transaction(name).objectStore(name).getAll();
      r.onsuccess = () => res(r.result);
    });
    out.push(name, all);
  }
  db.close();
  return JSON.stringify(out);
}

async function forceLastSeq(idb: IDBFactory, lib: string, last: bigint) {
  const db = await new Promise<IDBDatabase>((res) => {
    const r = idb.open(DB_NAME);
    r.onsuccess = () => res(r.result);
  });
  await new Promise<void>((res) => {
    const tx = db.transaction("replica", "readwrite");
    const s = tx.objectStore("replica");
    const g = s.get(lib);
    g.onsuccess = () => s.put({ ...g.result, last_client_seq: toSeq20(last), acked_client_seq: toSeq20(last) });
    tx.oncomplete = () => res();
  });
  db.close();
}

describe("replica store (IndexedDB)", () => {
  it("persists one S1 UUIDv7 ReplicaId per library; never regenerated across reopen", async () => {
    const { idb, store, mint, minted } = await setup();
    const a = await store.ensureReplica(LIB, mint);
    expect(a.replica_id).toMatch(S1);
    expect((await store.ensureReplica(LIB, mint)).replica_id).toBe(a.replica_id);
    store.close();
    const again = await ReplicaStore.open(idb);
    expect((await again.ensureReplica(LIB, mint)).replica_id).toBe(a.replica_id);
    expect(minted()).toBe(1);
    expect(a.last_client_seq).toBe("00000000000000000000");
  });

  it("commit allocates contiguous client_seq and stores the exact envelope; root_id = stroke_id", async () => {
    const { store, mint, enc } = await setup();
    const rec = await store.ensureReplica(LIB, mint);
    const ops = [];
    for (let i = 0; i < 3; i++) ops.push(await store.commitOp(LIB, await draft(i), enc));
    expect(ops.map((o) => o.client_seq)).toEqual([toSeq20(1n), toSeq20(2n), toSeq20(3n)]);
    const out = await store.outbox(LIB);
    expect(out.map((o) => o.envelope)).toEqual(ops.map((o) => o.envelope));
    const env = JSON.parse(out[1]!.envelope);
    expect(env.client_seq).toBe(2);
    expect(env.replica_id).toBe(rec.replica_id);
    expect(env.operation.type).toBe("add_stroke");
    expect(env.operation.stroke.root_id).toBe(env.operation.stroke.stroke_id);
    expect((await store.replica(LIB))!.last_client_seq).toBe(toSeq20(3n));
  });

  it("a failed encode consumes no sequence and writes nothing", async () => {
    const { store, mint, enc } = await setup();
    await store.ensureReplica(LIB, mint);
    const bad = await draft();
    bad.stroke.points = [{ x: -5, y: 1 }]; // off-page: the Rust bridge refuses it
    await expect(store.commitOp(LIB, bad, enc)).rejects.toBeInstanceOf(StoreError);
    expect(await store.pendingCount(LIB)).toBe(0);
    expect((await store.replica(LIB))!.last_client_seq).toBe(toSeq20(0n));
    expect((await store.commitOp(LIB, await draft(), enc)).client_seq).toBe(toSeq20(1n));
  });

  it("uses the full u64 range and refuses to wrap at u64::MAX", async () => {
    const { idb, store, mint, enc } = await setup();
    await store.ensureReplica(LIB, mint);
    await forceLastSeq(idb, LIB, U64_MAX - 1n);
    const last = await store.commitOp(LIB, await draft(), enc);
    expect(last.client_seq).toBe("18446744073709551615");
    expect(last.envelope).toContain('"client_seq":18446744073709551615,');
    const e = await store.commitOp(LIB, await draft(), enc).catch((x: unknown) => x);
    expect(e).toBeInstanceOf(StoreError);
    expect((e as StoreError).code).toBe(REPLICA_SEQUENCE_EXHAUSTED);
    expect(await store.pendingCount(LIB)).toBe(1);
  });

  it.each([9007199254740992n, 9223372036854775807n])("client_seq across %s stays exact", async (start) => {
    const { idb, store, mint, enc } = await setup();
    await store.ensureReplica(LIB, mint);
    await forceLastSeq(idb, LIB, start - 1n);
    const a = await store.commitOp(LIB, await draft(), enc);
    const b = await store.commitOp(LIB, await draft(), enc);
    expect(a.envelope).toContain(`"client_seq":${start},`);
    expect(b.envelope).toContain(`"client_seq":${start + 1n},`);
  });

  it("two tabs (two connections) never allocate the same client_seq", async () => {
    const { idb, store, mint, enc } = await setup();
    await store.ensureReplica(LIB, mint);
    const other = await ReplicaStore.open(idb);
    const drafts = await Promise.all(Array.from({ length: 40 }, (_, i) => draft(i)));
    const ops = await Promise.all(drafts.map((d, i) => (i % 2 ? store : other).commitOp(LIB, d, enc)));
    const seqs = ops.map((o) => o.client_seq).sort();
    expect(new Set(seqs).size).toBe(40);
    expect(seqs[0]).toBe(toSeq20(1n));
    expect(seqs[39]).toBe(toSeq20(40n));
  });

  it("ACK moves outbox → accepted atomically (bytes kept), idempotently; overlay and compaction", async () => {
    const { store, mint, enc } = await setup();
    await store.ensureReplica(LIB, mint);
    const ops = [];
    for (let i = 0; i < 3; i++) ops.push(await store.commitOp(LIB, await draft(i), enc));
    const acks = ops.slice(0, 2).map((o, i) => ({ client_seq: o.client_seq, operation_id: o.operation_id, server_seq: 9007199254740993n + BigInt(i) }));
    await store.recordAcks(LIB, acks);
    await store.recordAcks(LIB, acks); // a second tab recording the same ACK
    const acc = await store.accepted(LIB);
    expect(acc.map((a) => a.envelope)).toEqual(ops.slice(0, 2).map((o) => o.envelope));
    expect(acc[1]!.server_seq).toBe("00009007199254740994");
    expect((await store.outbox(LIB)).map((o) => o.client_seq)).toEqual([ops[2]!.client_seq]);
    expect((await store.replica(LIB))!.acked_client_seq).toBe(toSeq20(2n));
    // Overlay: accepted not yet in the snapshot, then outbox — client_seq order.
    expect((await store.overlay(LIB, 0n)).map((o) => o.client_seq)).toEqual(ops.map((o) => o.client_seq));
    expect((await store.overlay(LIB, 9007199254740993n)).map((o) => o.client_seq)).toEqual([ops[1]!.client_seq, ops[2]!.client_seq]);
    expect(await store.compact(LIB, 9007199254740993n)).toBe(1);
    expect((await store.accepted(LIB)).length).toBe(1);
    // A mismatched ACK is refused without touching anything.
    await expect(store.recordAcks(LIB, [{ client_seq: ops[2]!.client_seq, operation_id: ops[0]!.operation_id, server_seq: 5n }])).rejects.toBeInstanceOf(StoreError);
    expect(await store.pendingCount(LIB)).toBe(1);
  });

  it("conflict pauses commits and preserves the outbox", async () => {
    const { store, mint, enc } = await setup();
    await store.ensureReplica(LIB, mint);
    await store.commitOp(LIB, await draft(), enc);
    await store.setConflict(LIB, { client_seq: toSeq20(1n), operation_id: "x", status: 422, code: "c", message: "m" });
    const e = await store.commitOp(LIB, await draft(), enc).catch((x: unknown) => x);
    expect((e as StoreError).code).toBe("conflict");
    expect(await store.pendingCount(LIB)).toBe(1);
  });

  it("never stores the bearer token", async () => {
    const { idb, store, mint, enc } = await setup();
    const token = "f00d".repeat(16);
    await store.ensureReplica(LIB, mint);
    await store.commitOp(LIB, await draft(), enc);
    const dump = await dumpAll(idb);
    expect(dump).not.toContain(token);
    expect(dump).not.toMatch(/bearer|authorization|token/i);
  });
});

async function coordinator(server = new FakeServer(), opts: { random?: () => number } = {}) {
  const s = await setup();
  const timers: { fn: () => void; ms: number }[] = [];
  const refreshes: number[] = [];
  let unauthorized = 0;
  const c = new SyncCoordinator({
    store: s.store,
    api: server,
    libraryId: LIB,
    mintReplicaId: s.mint,
    encoder: s.enc,
    requestRefresh: () => refreshes.push(1),
    onUnauthorized: () => unauthorized++,
    setTimeout: (fn, ms) => (timers.push({ fn, ms }), timers.length),
    clearTimeout: () => undefined,
    random: opts.random ?? (() => 0.5),
  });
  await c.start();
  return { ...s, c, server, timers, refreshes, unauthorized: () => unauthorized };
}

describe("sync coordinator", () => {
  it("commit → one POST with the exact stored bytes → accepted; push-first gate", async () => {
    const t = await coordinator();
    let release!: () => void;
    t.server.gate = new Promise((r) => (release = r));
    const op = await t.c.commit(await draft());
    expect(op).not.toBeNull();
    expect(t.c.canRefresh()).toBe(false); // unACKed: the snapshot must not be refreshed
    expect(t.c.state.overlay).toHaveLength(1);
    release();
    await t.c.idle();
    expect(t.server.bodies).toEqual([`{"envelopes":[${op!.envelope}]}`]);
    expect(t.c.state.pending).toBe(0);
    expect(t.c.state.phase).toBe("synced");
    expect(t.c.canRefresh()).toBe(true);
    expect(t.refreshes.length).toBe(1);
    expect((await t.store.accepted(LIB))[0]!.envelope).toBe(op!.envelope);
    expect(t.c.state.overlay[0]!.server_seq).toBe(1n);
  });

  it("batches at most 50 in exact client_seq order", async () => {
    const t = await coordinator();
    let release!: () => void;
    t.server.gate = new Promise((r) => (release = r));
    for (let i = 0; i < 120; i++) await t.c.commit(await draft(i));
    release();
    await t.c.idle();
    const sizes = t.server.bodies.map((b) => (b.match(/"add_stroke"/g) ?? []).length);
    expect(sizes.every((n) => n <= 50)).toBe(true);
    expect(sizes.reduce((a, b) => a + b, 0)).toBeGreaterThanOrEqual(120);
    const seqs = t.server.bodies.flatMap((b) => [...b.matchAll(/"client_seq":(\d+)/g)].map((m) => Number(m[1])));
    const unique = [...new Set(seqs)];
    expect(unique).toEqual(Array.from({ length: 120 }, (_, i) => i + 1));
    expect(t.server.head).toBe(120n);
    expect(t.c.state.pending).toBe(0);
  });

  it("lost ACK: retries the SAME bytes; the server dedups (no double apply)", async () => {
    const t = await coordinator();
    t.server.faults = ["lost-ack"];
    await t.c.commit(await draft());
    await t.c.idle();
    expect(t.c.state.phase).toBe("offline");
    expect(t.c.state.pending).toBe(1);
    expect(t.c.canRefresh()).toBe(false);
    expect(t.timers).toHaveLength(1);
    t.timers[0]!.fn();
    await t.c.idle();
    expect(t.server.bodies).toHaveLength(2);
    expect(t.server.bodies[1]).toBe(t.server.bodies[0]);
    expect(t.server.head).toBe(1n);
    expect(t.c.state.pending).toBe(0);
    expect((await t.store.accepted(LIB))[0]!.server_seq).toBe(toSeq20(1n));
  });

  it("offline: strokes keep queuing and all sync in order when the server returns", async () => {
    const t = await coordinator();
    t.server.faults = ["unreachable", "unreachable"];
    await t.c.commit(await draft(1));
    await t.c.idle();
    await t.c.commit(await draft(2));
    await t.c.idle();
    expect(t.c.state.phase).toBe("offline");
    expect(t.c.state.pending).toBe(2);
    t.c.online();
    await t.c.idle();
    expect(t.c.state.pending).toBe(0);
    expect(t.server.head).toBe(2n);
  });

  it("401 locks and preserves the outbox", async () => {
    const t = await coordinator();
    t.server.faults = ["401"];
    await t.c.commit(await draft());
    await t.c.idle();
    expect(t.unauthorized()).toBe(1);
    expect(t.c.state.phase).toBe("locked");
    expect(await t.store.pendingCount(LIB)).toBe(1);
  });

  it("a semantic refusal fails closed: conflict recorded, outbox preserved, no further sends", async () => {
    const t = await coordinator();
    t.server.faults = ["422"];
    await t.c.commit(await draft());
    await t.c.idle();
    expect(t.c.state.phase).toBe("conflict");
    expect(t.c.state.conflict!.code).toBe("unknown_page");
    expect(conflictText(t.c.state.conflict!)).toContain("Your pending operation has been preserved.");
    expect(conflictText(t.c.state.conflict!)).toContain("SYNC CONFLICT");
    expect(await t.store.pendingCount(LIB)).toBe(1);
    expect(await t.c.commit(await draft())).toBeNull();
    t.c.kick();
    await t.c.idle();
    expect(t.server.bodies).toHaveLength(1);
    expect(t.timers).toHaveLength(0);
  });

  it("server sequences above 2^53 stay exact through ACK and compaction", async () => {
    const t = await coordinator(new FakeServer(9223372036854775806n));
    await t.c.commit(await draft());
    await t.c.idle();
    expect((await t.store.accepted(LIB))[0]!.server_seq).toBe("09223372036854775807");
    expect(t.c.state.overlay[0]!.server_seq).toBe(9223372036854775807n);
    await t.c.snapshotApplied(9223372036854775806n);
    expect(await t.store.accepted(LIB)).toHaveLength(1);
    await t.c.snapshotApplied(9223372036854775807n);
    expect(await t.store.accepted(LIB)).toHaveLength(0);
    expect(t.c.state.overlay).toHaveLength(0);
  });

  it("stop() then idle() then close() does not leave push issuing transactions on a closing DB", async () => {
    const t = await coordinator();
    let release!: () => void;
    t.server.gate = new Promise((r) => (release = r));
    await t.c.commit(await draft());
    t.c.stop();
    const idle = t.c.idle();
    release();
    await idle;
    t.store.close();
    await expect(t.store.outbox(LIB)).rejects.toThrow();
  });

  it("a reopened tab resumes pushing what an earlier visit left queued", async () => {
    const server = new FakeServer();
    const t = await coordinator(server);
    server.faults = ["unreachable"];
    await t.c.commit(await draft());
    await t.c.idle();
    t.c.stop();
    t.store.close();
    const store = await ReplicaStore.open(t.idb);
    const c2 = new SyncCoordinator({
      store,
      api: server,
      libraryId: LIB,
      mintReplicaId: () => {
        throw new Error("must not mint again");
      },
      encoder: t.enc,
      requestRefresh: () => undefined,
      onUnauthorized: () => undefined,
      setTimeout: () => 0,
      clearTimeout: () => undefined,
      random: () => 0.5,
    });
    await c2.start();
    await c2.idle();
    expect(server.head).toBe(1n);
    expect(c2.state.pending).toBe(0);
  });

  it("backoff grows, is capped, and is jittered", () => {
    expect(backoffDelay(0, () => 0)).toBe(500);
    expect(backoffDelay(0, () => 0.999)).toBeLessThanOrEqual(1000);
    expect(backoffDelay(3, () => 0.5)).toBe(6000);
    expect(backoffDelay(50, () => 0.999)).toBeLessThanOrEqual(BACKOFF_MAX_MS);
    expect(backoffDelay(50, () => 0)).toBe(BACKOFF_MAX_MS / 2);
  });
});
