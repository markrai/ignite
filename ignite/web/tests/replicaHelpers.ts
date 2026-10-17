// Shared S3B test helpers: the real WASM bridge, a fresh fake IndexedDB, and an in-memory
// server that follows the ledger's rules (dedup by OperationId, per-replica client_seq = last+1).

import "fake-indexeddb/auto";
import { IDBFactory } from "fake-indexeddb";
import { readFileSync } from "node:fs";
import { RefusedError, TransientError, type ReplicaApi, type SubmitAck } from "../src/api/replicaApi";
import { bridgeEncoder } from "../src/replica/encoder";
import type { EraseDraft, StrokeDraft, Encoder } from "../src/replica/store";
import { ProtocolBridge } from "../src/wasm/bridge";

export const LIB = "01a0c450-6c01-7101-8203-040506070801";
export const NB = "01a0c450-6c04-7401-8203-040506070804";
export const PG = "01a0c450-6c05-7501-8203-040506070805";

const wasm = readFileSync(new URL("../src/wasm/scribe_web_wasm.wasm", import.meta.url));
let bridgeP: Promise<ProtocolBridge> | null = null;
export function bridge(): Promise<ProtocolBridge> {
  bridgeP ??= ProtocolBridge.instantiate(new Uint8Array(wasm));
  return bridgeP;
}

export function freshIdb(): IDBFactory {
  return new IDBFactory();
}

export async function encoderFor(lib = LIB): Promise<Encoder> {
  return bridgeEncoder(await bridge(), lib);
}

/** An erase of `roots` along a short horizontal sweep through (x, 300). */
export async function eraseDraft(roots: string[], x = 200, page = PG): Promise<EraseDraft> {
  const b = await bridge();
  return {
    kind: "erase",
    operation_id: b.mintId("operation"),
    notebook_id: NB,
    page_id: page,
    erase: { observed_roots: roots, path: [{ x, y: 100 }, { x, y: 500 }] },
  };
}

export async function draft(n = 0, page = PG): Promise<StrokeDraft> {
  const b = await bridge();
  const sid = b.mintId("stroke");
  return {
    operation_id: b.mintId("operation"),
    notebook_id: NB,
    page_id: page,
    stroke: { stroke_id: sid, root_id: sid, style: "legacy-fixed-v1", points: [{ x: 100 + n, y: 200 }, { x: 300 + n, y: 400 }] },
  };
}

/** Ledger-faithful fake server. `faults` lets a test inject one-shot failures. */
export class FakeServer implements ReplicaApi {
  head: bigint;
  readonly bodies: string[] = [];
  readonly byOp = new Map<string, bigint>();
  readonly lastSeq = new Map<string, bigint>();
  faults: ("lost-ack" | "unreachable" | "401" | "422" | "409")[] = [];
  gate: Promise<void> | null = null;

  constructor(start = 0n) {
    this.head = start;
  }

  async submit(libraryId: string, batch: readonly { operation_id: string; envelope: string }[]): Promise<SubmitAck> {
    if (this.gate) await this.gate;
    const body = `{"envelopes":[${batch.map((e) => e.envelope).join(",")}]}`;
    this.bodies.push(body);
    const f = this.faults.shift();
    if (f === "unreachable") throw new TransientError(0, "unreachable");
    if (f === "401") {
      const { ReplicaUnauthorizedError } = await import("../src/api/replicaApi");
      throw new ReplicaUnauthorizedError();
    }
    if (f === "422") throw new RefusedError(422, "unknown_page", "page never existed", 0);
    if (f === "409") throw new RefusedError(409, "client_seq_out_of_order", "gap", null);
    const results = batch.map((e) => {
      const prior = this.byOp.get(e.operation_id);
      if (prior !== undefined) return { operation_id: e.operation_id, server_seq: prior, outcome: "applied" as const, duplicate: true };
      const replica = /"replica_id":"([^"]+)"/.exec(e.envelope)![1]!;
      const seq = BigInt(/"client_seq":(\d+)/.exec(e.envelope)![1]!);
      const last = this.lastSeq.get(replica) ?? 0n;
      if (seq !== last + 1n) throw new RefusedError(409, "client_seq_out_of_order", `expected ${last + 1n} got ${seq}`, null);
      this.lastSeq.set(replica, seq);
      this.head += 1n;
      this.byOp.set(e.operation_id, this.head);
      return { operation_id: e.operation_id, server_seq: this.head, outcome: "applied" as const, duplicate: false };
    });
    if (libraryId !== LIB) throw new RefusedError(404, "library_not_found", "no library", null);
    if (f === "lost-ack") throw new TransientError(0, "response lost");
    return { head_server_seq: this.head, results };
  }
}
