// S3B durable browser replica state in IndexedDB (`scribe-web-sync`).
//
// Stores (all keyed per library; this browser profile is one replica per library):
//   replica   {library_id, replica_id, last_client_seq, acked_client_seq, conflict}
//   outbox    [library_id, client_seq] → exact canonical envelope text, not yet ACKed
//   accepted  [library_id, client_seq] → same, plus the server_seq the server assigned
//
// Sequences are SEQ20 strings (full u64, order-preserving). The bearer token is NEVER stored
// here — it lives only in sessionStorage.
//
// Invariants:
//   * A stroke commit is ONE readwrite transaction over {replica, outbox}: read
//     last_client_seq, encode the envelope (synchronous WASM), write the outbox row and
//     advance last_client_seq. Any failure aborts it: no sequence is consumed, nothing written.
//     IndexedDB serializes overlapping readwrite transactions, so two tabs can never allocate
//     the same client_seq.
//   * Outbox rows are immutable: the exact text is resent verbatim until ACKed (lost-ACK
//     retries are deduplicated by the server by OperationId).
//   * ACK moves outbox → accepted atomically (bytes kept) and advances acked_client_seq.
//   * Accepted rows are compacted once a displayed snapshot's through_server_seq ≥ server_seq.

import type { PageSpec, SyncStroke, SyncText } from "../model/types";
import { fromSeq20, nextSeq, REPLICA_SEQUENCE_EXHAUSTED, SequenceError, toSeq20 } from "./seq";

export const DB_NAME = "scribe-web-sync";
const DB_VERSION = 1;

export interface ConflictInfo {
  client_seq: string;
  operation_id: string;
  status: number;
  code: string;
  message: string;
}

export interface ReplicaRecord {
  library_id: string;
  replica_id: string;
  /** SEQ20 of the last allocated client_seq ("000…0" before the first stroke). */
  last_client_seq: string;
  /** SEQ20 of the last client_seq the server ACKed. */
  acked_client_seq: string;
  /** Fail-closed state: set on a semantic/sequencing refusal. Pending rows are preserved. */
  conflict: ConflictInfo | null;
}

/** The semantic intent of one erase gesture (`EraseRegion`; radius is fixed by Rust). */
export interface EraseIntent {
  /** Lineage roots of every stroke visible on the page at pointer down. */
  observed_roots: string[];
  /** Down point followed by each sample, canonical page units (no pressure). */
  path: { x: number; y: number }[];
}

interface DraftBase {
  operation_id: string;
  notebook_id: string;
  page_id: string;
}

/** AddStroke. `kind` is absent on rows written before browser erase existed. */
export interface StrokeDraft extends DraftBase {
  kind?: "add";
  /** Presentation copy of the stroke carried by the envelope (overlay rendering only). */
  stroke: SyncStroke;
}

export interface EraseDraft extends DraftBase {
  kind: "erase";
  /** Presentation copy of the intent carried by the envelope (overlay projection only). */
  erase: EraseIntent;
}

/** One library-level folder operation (the wire `SyncOperationV1` payload). */
export type FolderIntent =
  | { type: "create_folder"; folder_id: string; title: string }
  | { type: "rename_folder"; folder_id: string; title: string }
  | { type: "delete_folder"; folder_id: string }
  | { type: "set_notebook_folder"; notebook_id: string; folder_id?: string };

/** A folder operation. Library-level: `notebook_id`/`page_id` are empty (no page). */
export interface FolderDraft extends DraftBase {
  kind: "folder";
  folder: FolderIntent;
}

/** Notebook/page structure (wire `SyncOperationV1` payload, minus `move_page`). */
export type StructureIntent =
  | { type: "create_notebook"; notebook_id: string; title: string; after_notebook_id?: string; first_page: PageSpec }
  | { type: "rename_notebook"; notebook_id: string; title: string }
  | { type: "delete_notebook"; notebook_id: string }
  | { type: "create_page"; notebook_id: string; page: PageSpec; after_page_id?: string }
  | { type: "delete_page"; notebook_id: string; page_id: string };

export interface StructureDraft extends DraftBase {
  kind: "structure";
  structure: StructureIntent;
}

export interface UndoDraft extends DraftBase {
  kind: "undo";
  /** Lineage root of the newest visible stroke (may differ from a fragment's stroke_id). */
  root_id: string;
}

export interface TextAddDraft extends DraftBase {
  kind: "add_text";
  text: SyncText;
}

export interface TextUpdateDraft extends DraftBase {
  kind: "update_text";
  text: SyncText;
}

export interface TextDeleteDraft extends DraftBase {
  kind: "delete_text";
  text_id: string;
}

/** What the commit needs to build the envelope, minus the sequence it allocates. */
export type OpDraft =
  | StrokeDraft
  | EraseDraft
  | FolderDraft
  | StructureDraft
  | UndoDraft
  | TextAddDraft
  | TextUpdateDraft
  | TextDeleteDraft;

export type PendingOp = OpDraft & {
  library_id: string;
  client_seq: string;
  /** Exact canonical SyncEnvelopeV1 text from the Rust bridge. Never re-encoded. */
  envelope: string;
};

export type AcceptedOp = PendingOp & { server_seq: string };

export function isErase(op: OpDraft): op is EraseDraft {
  return op.kind === "erase";
}

export function isFolder(op: OpDraft): op is FolderDraft {
  return op.kind === "folder";
}

export function isStructure(op: OpDraft): op is StructureDraft {
  return op.kind === "structure";
}

export function isUndo(op: OpDraft): op is UndoDraft {
  return op.kind === "undo";
}

export function isTextAdd(op: OpDraft): op is TextAddDraft {
  return op.kind === "add_text";
}

export function isTextUpdate(op: OpDraft): op is TextUpdateDraft {
  return op.kind === "update_text";
}

export function isTextDelete(op: OpDraft): op is TextDeleteDraft {
  return op.kind === "delete_text";
}

export function isTextOp(op: OpDraft): op is TextAddDraft | TextUpdateDraft | TextDeleteDraft {
  return op.kind === "add_text" || op.kind === "update_text" || op.kind === "delete_text";
}

/** Synchronous envelope encoder (the WASM bridge). Throws on refusal. */
export type Encoder = (replicaId: string, clientSeq: string, draft: OpDraft) => string;

export class StoreError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "StoreError";
  }
}

const ZERO = toSeq20(0n);
const OPEN_RETRIES = 8;

function isRetryableOpen(e: unknown): boolean {
  const name = e instanceof DOMException ? e.name : "";
  const msg = e instanceof Error ? e.message : String(e);
  return name === "InvalidStateError" || name === "AbortError" || /closing|InvalidStateError|AbortError/i.test(msg);
}

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function req<T>(r: IDBRequest<T>): Promise<T> {
  return new Promise((res, rej) => {
    r.onsuccess = () => res(r.result);
    r.onerror = () => rej(r.error);
  });
}

function done(tx: IDBTransaction): Promise<void> {
  return new Promise((res, rej) => {
    tx.oncomplete = () => res();
    tx.onabort = () => rej(tx.error ?? new StoreError("aborted", "transaction aborted"));
    tx.onerror = () => undefined; // reported through onabort
  });
}

function range(lib: string): IDBKeyRange {
  return IDBKeyRange.bound([lib, ZERO], [lib, "99999999999999999999"]);
}

export class ReplicaStore {
  private closed = false;

  private constructor(private readonly db: IDBDatabase) {}

  static async open(factory: IDBFactory = indexedDB): Promise<ReplicaStore> {
    let last: unknown;
    for (let i = 0; i < OPEN_RETRIES; i++) {
      try {
        const open = factory.open(DB_NAME, DB_VERSION);
        open.onupgradeneeded = () => {
          const db = open.result;
          if (!db.objectStoreNames.contains("replica")) db.createObjectStore("replica", { keyPath: "library_id" });
          if (!db.objectStoreNames.contains("outbox")) db.createObjectStore("outbox", { keyPath: ["library_id", "client_seq"] });
          if (!db.objectStoreNames.contains("accepted")) db.createObjectStore("accepted", { keyPath: ["library_id", "client_seq"] });
        };
        const db = await req(open);
        return new ReplicaStore(db);
      } catch (e) {
        last = e;
        if (!isRetryableOpen(e) || i === OPEN_RETRIES - 1) throw e;
        await delay(40 * (i + 1));
      }
    }
    throw last instanceof Error ? last : new StoreError("save_failed", String(last));
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.db.close();
  }

  async replica(lib: string): Promise<ReplicaRecord | null> {
    const tx = this.db.transaction("replica", "readonly");
    return ((await req(tx.objectStore("replica").get(lib))) as ReplicaRecord | undefined) ?? null;
  }

  /** The persistent ReplicaId for `lib`; `mint` runs only if none exists yet (never regenerated). */
  async ensureReplica(lib: string, mint: () => string): Promise<ReplicaRecord> {
    const existing = await this.replica(lib);
    if (existing) return existing;
    const candidate = mint();
    const tx = this.db.transaction("replica", "readwrite");
    const s = tx.objectStore("replica");
    let out: ReplicaRecord | null = null;
    const g = s.get(lib);
    g.onsuccess = () => {
      const cur = g.result as ReplicaRecord | undefined;
      if (cur) {
        out = cur; // another tab won the race; keep its identity
        return;
      }
      out = { library_id: lib, replica_id: candidate, last_client_seq: ZERO, acked_client_seq: ZERO, conflict: null };
      s.put(out);
    };
    await done(tx);
    return out!;
  }

  /**
   * Atomically allocate the next client_seq, encode, and durably queue one operation.
   * Throws `StoreError` (nothing consumed) on conflict, exhaustion or encoder refusal.
   */
  async commitOp(lib: string, draft: OpDraft, encode: Encoder): Promise<PendingOp> {
    const tx = this.db.transaction(["replica", "outbox"], "readwrite");
    const rs = tx.objectStore("replica");
    const os = tx.objectStore("outbox");
    let failure: unknown = null;
    let op: PendingOp | null = null;
    const g = rs.get(lib);
    g.onsuccess = () => {
      try {
        const rec = g.result as ReplicaRecord | undefined;
        if (!rec) throw new StoreError("no_replica", "No browser replica for this library.");
        if (rec.conflict) throw new StoreError("conflict", "Sync conflict: writing is paused.");
        const next = nextSeq(fromSeq20(rec.last_client_seq));
        if (next === null) throw new StoreError(REPLICA_SEQUENCE_EXHAUSTED, "This browser replica has exhausted its sequence space.");
        const seq = toSeq20(next);
        const envelope = encode(rec.replica_id, seq, draft);
        op = { library_id: lib, client_seq: seq, ...draft, envelope };
        os.add(op);
        rs.put({ ...rec, last_client_seq: seq });
      } catch (e) {
        failure = e;
        tx.abort();
      }
    };
    try {
      await done(tx);
    } catch (e) {
      if (failure instanceof StoreError || failure instanceof SequenceError) throw failure;
      throw new StoreError("save_failed", failure instanceof Error ? failure.message : e instanceof Error ? e.message : String(e));
    }
    return op!;
  }

  async outbox(lib: string, limit?: number): Promise<PendingOp[]> {
    const tx = this.db.transaction("outbox", "readonly");
    return (await req(tx.objectStore("outbox").getAll(range(lib), limit))) as PendingOp[];
  }

  async accepted(lib: string): Promise<AcceptedOp[]> {
    const tx = this.db.transaction("accepted", "readonly");
    return (await req(tx.objectStore("accepted").getAll(range(lib)))) as AcceptedOp[];
  }

  async pendingCount(lib: string): Promise<number> {
    const tx = this.db.transaction("outbox", "readonly");
    return req(tx.objectStore("outbox").count(range(lib)));
  }

  /**
   * Record a server ACK for a batch: each outbox row → accepted with its server_seq, and
   * acked_client_seq advances. Idempotent (another tab may already have recorded it).
   * `acks` pairs each sent row with its result, in client_seq order.
   */
  async recordAcks(lib: string, acks: { client_seq: string; operation_id: string; server_seq: bigint }[]): Promise<void> {
    const tx = this.db.transaction(["replica", "outbox", "accepted"], "readwrite");
    const rs = tx.objectStore("replica");
    const os = tx.objectStore("outbox");
    const as = tx.objectStore("accepted");
    let failure: unknown = null;
    const g = rs.get(lib);
    g.onsuccess = () => {
      const rec = g.result as ReplicaRecord;
      let acked = rec.acked_client_seq;
      let pending = acks.length;
      if (pending === 0) return;
      for (const a of acks) {
        const r = os.get([lib, a.client_seq]);
        r.onsuccess = () => {
          const row = r.result as PendingOp | undefined;
          if (row) {
            if (row.operation_id !== a.operation_id) {
              failure = new StoreError("ack_mismatch", `ACK operation ${a.operation_id} does not match client_seq ${a.client_seq}`);
              tx.abort();
              return;
            }
            as.put({ ...row, server_seq: toSeq20(a.server_seq) });
            os.delete([lib, a.client_seq]);
          }
          if (a.client_seq > acked) acked = a.client_seq;
          if (--pending === 0) rs.put({ ...rec, acked_client_seq: acked });
        };
      }
    };
    try {
      await done(tx);
    } catch (e) {
      throw failure ?? e;
    }
  }

  /** Drop accepted rows the displayed snapshot already contains. */
  async compact(lib: string, throughServerSeq: bigint): Promise<number> {
    const through = toSeq20(throughServerSeq);
    const tx = this.db.transaction("accepted", "readwrite");
    const s = tx.objectStore("accepted");
    let n = 0;
    const cur = s.openCursor(range(lib));
    cur.onsuccess = () => {
      const c = cur.result;
      if (!c) return;
      if ((c.value as AcceptedOp).server_seq <= through) {
        c.delete();
        n++;
      }
      c.continue();
    };
    await done(tx);
    return n;
  }

  /** Fail closed. The outbox is preserved untouched. */
  async setConflict(lib: string, conflict: ConflictInfo | null): Promise<void> {
    const tx = this.db.transaction("replica", "readwrite");
    const s = tx.objectStore("replica");
    const g = s.get(lib);
    g.onsuccess = () => {
      const rec = g.result as ReplicaRecord | undefined;
      if (rec) s.put({ ...rec, conflict });
    };
    await done(tx);
  }

  /**
   * The durable pending overlay for the displayed snapshot: accepted-but-not-yet-visible rows,
   * then unACKed outbox rows — in client_seq order.
   */
  async overlay(lib: string, throughServerSeq: bigint): Promise<PendingOp[]> {
    const through = toSeq20(throughServerSeq);
    const tx = this.db.transaction(["accepted", "outbox"], "readonly");
    const [acc, out] = await Promise.all([
      req(tx.objectStore("accepted").getAll(range(lib))) as Promise<AcceptedOp[]>,
      req(tx.objectStore("outbox").getAll(range(lib))) as Promise<PendingOp[]>,
    ]);
    return [...acc.filter((a) => a.server_seq > through), ...out].sort((a, b) => (a.client_seq < b.client_seq ? -1 : a.client_seq > b.client_seq ? 1 : 0));
  }
}
