// S3B sync coordinator: the one owner of browser → server replication for a library.
//
//   pointer up → commit(draft)            one IndexedDB transaction (seq + exact bytes)
//   push loop  → POST ≤ MAX_BATCH rows     exact client_seq order, starting at acked + 1
//   ACK        → outbox → accepted         one transaction; bytes kept; acked advances
//   snapshot   → compact accepted          once through_server_seq ≥ server_seq
//
// Push-first: while unACKed rows exist the library session must not refresh its snapshot
// (`canRefresh()`), so the view never regresses behind strokes the server may already have.
// Failures: network/5xx → retry the SAME bytes with jittered backoff (a lost ACK is
// deduplicated by the server by OperationId); 401 → stop and lock, outbox preserved;
// a definitive refusal → fail closed ("SYNC CONFLICT"), outbox preserved, no further sends.
//
// Cross-tab: sequence allocation is atomic in IndexedDB; pushes are serialized with Web Locks
// where available, and tabs nudge each other over a BroadcastChannel. Duplicate sends from two
// tabs are harmless (server dedup; ACK recording is idempotent).

import { MAX_BATCH, RefusedError, ReplicaUnauthorizedError, TransientError, type ReplicaApi } from "../api/replicaApi";
import type { SyncStroke, SyncText } from "../model/types";
import { fromSeq20, REPLICA_SEQUENCE_EXHAUSTED, toSeq20 } from "./seq";
import {
  isErase,
  isFolder,
  isStructure,
  isTextAdd,
  isTextDelete,
  isTextUpdate,
  isUndo,
  StoreError,
  type ConflictInfo,
  type EraseIntent,
  type Encoder,
  type FolderIntent,
  type OpDraft,
  type PendingOp,
  type ReplicaStore,
  type StructureIntent,
} from "./store";

/** Keep each POST body well under the server's 4 MiB JSON limit. */
export const MAX_BATCH_BYTES = 3 * 1024 * 1024;
export const BACKOFF_MIN_MS = 1000;
export const BACKOFF_MAX_MS = 30_000;

export type SyncPhase = "idle" | "saved" | "syncing" | "offline" | "synced" | "conflict" | "locked";

interface OverlayBase {
  client_seq: string;
  operation_id: string;
  notebook_id: string;
  page_id: string;
  /** Set once ACKed: the op leaves the overlay when the displayed snapshot is through this. */
  server_seq: bigint | null;
}

/** One durable pending operation, projected over the displayed snapshot in client_seq order. */
export type OverlayOp =
  | (OverlayBase & { kind: "add"; stroke: SyncStroke })
  | (OverlayBase & { kind: "erase"; erase: EraseIntent })
  | (OverlayBase & { kind: "folder"; folder: FolderIntent })
  | (OverlayBase & { kind: "structure"; structure: StructureIntent })
  | (OverlayBase & { kind: "undo"; root_id: string })
  | (OverlayBase & { kind: "add_text"; text: SyncText })
  | (OverlayBase & { kind: "update_text"; text: SyncText })
  | (OverlayBase & { kind: "delete_text"; text_id: string });

export interface CoordinatorState {
  ready: boolean;
  replicaId: string | null;
  phase: SyncPhase;
  pending: number;
  conflict: ConflictInfo | null;
  overlay: OverlayOp[];
  /** Restrained, user-facing message for the last failed save (nothing was queued). */
  saveError: string | null;
}

export interface CoordinatorDeps {
  store: ReplicaStore;
  api: ReplicaApi;
  libraryId: string;
  mintReplicaId: () => string;
  encoder: Encoder;
  /** Ask the library session to poll now (after an ACK). */
  requestRefresh: () => void;
  onUnauthorized: () => void;
  setTimeout?: (fn: () => void, ms: number) => unknown;
  clearTimeout?: (h: unknown) => void;
  /** Uniform [0, 1) from a CSPRNG (jitter only; never used for identity). */
  random?: () => number;
  channel?: BroadcastLike | null;
  locks?: LocksLike | null;
}

export interface BroadcastLike {
  postMessage(m: unknown): void;
  onmessage: ((e: MessageEvent) => void) | null;
  close(): void;
}

export interface LocksLike {
  request<T>(name: string, cb: () => Promise<T>): Promise<T>;
}

function cryptoRandom(): number {
  const a = new Uint32Array(1);
  crypto.getRandomValues(a);
  return a[0]! / 2 ** 32;
}

export function backoffDelay(attempt: number, random: () => number): number {
  const base = Math.min(BACKOFF_MAX_MS, BACKOFF_MIN_MS * 2 ** Math.min(attempt, 10));
  return Math.round(base / 2 + (random() * base) / 2);
}

export function conflictText(c: ConflictInfo): string {
  return `SYNC CONFLICT — the server refused a pending operation (${c.code}). Your pending operation has been preserved.`;
}

function toOverlay(rows: (PendingOp & { server_seq?: string })[]): OverlayOp[] {
  return rows.map((r) => {
    const base: OverlayBase = {
      client_seq: r.client_seq,
      operation_id: r.operation_id,
      notebook_id: r.notebook_id,
      page_id: r.page_id,
      server_seq: r.server_seq === undefined ? null : fromSeq20(r.server_seq),
    };
    if (isFolder(r)) return { ...base, kind: "folder", folder: r.folder };
    if (isStructure(r)) return { ...base, kind: "structure", structure: r.structure };
    if (isUndo(r)) return { ...base, kind: "undo", root_id: r.root_id };
    if (isTextAdd(r)) return { ...base, kind: "add_text", text: r.text };
    if (isTextUpdate(r)) return { ...base, kind: "update_text", text: r.text };
    if (isTextDelete(r)) return { ...base, kind: "delete_text", text_id: r.text_id };
    return isErase(r) ? { ...base, kind: "erase", erase: r.erase } : { ...base, kind: "add", stroke: r.stroke };
  });
}

export class SyncCoordinator {
  state: CoordinatorState = { ready: false, replicaId: null, phase: "idle", pending: 0, conflict: null, overlay: [], saveError: null };
  private listeners = new Set<() => void>();
  private pushing: Promise<void> | null = null;
  private again = false;
  private timer: unknown = null;
  private attempt = 0;
  private stopped = false;
  private offline = false;
  private through = 0n;
  private readonly setT: (fn: () => void, ms: number) => unknown;
  private readonly clearT: (h: unknown) => void;
  private readonly random: () => number;

  constructor(private readonly d: CoordinatorDeps) {
    this.setT = d.setTimeout ?? ((fn, ms) => globalThis.setTimeout(fn, ms));
    this.clearT = d.clearTimeout ?? ((h) => globalThis.clearTimeout(h as ReturnType<typeof setTimeout>));
    this.random = d.random ?? cryptoRandom;
    if (d.channel) {
      d.channel.onmessage = () => {
        void this.reload().then(() => this.kick());
      };
    }
  }

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private set(patch: Partial<CoordinatorState>): void {
    this.state = { ...this.state, ...patch };
    for (const l of this.listeners) l();
  }

  /** Push-first gate for the library session. */
  canRefresh(): boolean {
    return this.state.pending === 0 || this.state.conflict !== null;
  }

  /** Load durable state; resume pushing anything left from an earlier visit. */
  async start(): Promise<void> {
    this.stopped = false;
    await this.reload();
    this.set({ ready: true });
    this.kick();
  }

  /**
   * Stop pushing. Does not close the store: the owner must `await idle()` first so an
   * in-flight `outbox` transaction cannot outlive the connection.
   */
  stop(): void {
    this.stopped = true;
    this.again = false;
    if (this.timer !== null) this.clearT(this.timer);
    this.timer = null;
    this.d.channel?.close();
  }

  /** The persistent browser ReplicaId (minted once, on first entry to write mode). */
  async ensureReplica(): Promise<string> {
    const rec = await this.d.store.ensureReplica(this.d.libraryId, this.d.mintReplicaId);
    this.set({ replicaId: rec.replica_id, conflict: rec.conflict });
    return rec.replica_id;
  }

  private async reload(): Promise<void> {
    if (this.stopped) return;
    const lib = this.d.libraryId;
    const [rec, overlay, pending] = await Promise.all([
      this.d.store.replica(lib),
      this.d.store.overlay(lib, 0n),
      this.d.store.pendingCount(lib),
    ]);
    if (this.stopped) return;
    const conflict = rec?.conflict ?? null;
    this.set({
      replicaId: rec?.replica_id ?? null,
      conflict,
      pending,
      overlay: toOverlay(overlay),
      phase: this.phaseFor(pending, conflict),
    });
  }

  private phaseFor(pending: number, conflict: ConflictInfo | null): SyncPhase {
    if (conflict) return "conflict";
    if (this.state.phase === "locked") return "locked";
    if (pending === 0) return this.state.ready || this.state.overlay.length > 0 ? "synced" : "idle";
    if (this.offline) return "offline";
    return this.pushing ? "syncing" : "saved";
  }

  /**
   * Pointer up: durably queue exactly one AddStroke or EraseRegion. Resolves once it is in IndexedDB; the
   * send happens asynchronously. On failure nothing is queued and no sequence is consumed.
   */
  async commit(draft: OpDraft): Promise<PendingOp | null> {
    if (this.state.conflict) {
      this.set({ saveError: "Writing is paused by a sync conflict." });
      return null;
    }
    try {
      await this.ensureReplica();
      const op = await this.d.store.commitOp(this.d.libraryId, draft, this.d.encoder);
      const pending = this.state.pending + 1;
      this.set({
        saveError: null,
        pending,
        overlay: [...this.state.overlay, ...toOverlay([op])],
        phase: this.phaseFor(pending, null),
      });
      this.d.channel?.postMessage("changed");
      this.kick();
      return op;
    } catch (e) {
      const msg =
        e instanceof StoreError && e.code === REPLICA_SEQUENCE_EXHAUSTED
          ? "This browser can’t save more strokes for this library (sequence exhausted)."
          : e instanceof StoreError && e.code === "conflict"
            ? "Writing is paused by a sync conflict."
            : draft.kind === "folder"
            ? "Couldn’t save that folder change on this device. Nothing was changed."
            : draft.kind === "structure"
            ? "Couldn’t save that library change on this device. Nothing was changed."
            : draft.kind === "undo"
            ? "Couldn’t undo on this device. Nothing was changed."
            : "Couldn’t save that stroke on this device. It was not added.";
      this.set({ saveError: msg });
      return null;
    }
  }

  /** The library session displayed a snapshot through `through`: compact what it contains. */
  async snapshotApplied(through: bigint): Promise<void> {
    if (this.stopped) return;
    if (through <= this.through && this.through !== 0n) return;
    this.through = through;
    await this.d.store.compact(this.d.libraryId, through);
    if (this.stopped) return;
    await this.reload();
  }

  /** Browser came back online / a tab nudged us: try now. */
  online(): void {
    this.attempt = 0;
    this.kick();
  }

  /** Start (or coalesce into) a push pass. */
  kick(): void {
    if (this.stopped || this.state.phase === "locked" || this.state.conflict) return;
    if (this.timer !== null) {
      this.clearT(this.timer);
      this.timer = null;
    }
    if (this.pushing) {
      this.again = true;
      return;
    }
    this.pushing = this.runPush().finally(() => {
      this.pushing = null;
      if (this.again && !this.stopped) {
        this.again = false;
        this.kick();
      }
    });
  }

  /** Test hook: resolves when the current push pass (if any) finishes. */
  async idle(): Promise<void> {
    while (this.pushing) await this.pushing;
  }

  private async runPush(): Promise<void> {
    if (this.stopped) return;
    const name = `scribe-web-sync:${this.d.libraryId}`;
    const body = () => this.pushAll();
    try {
      if (this.d.locks) await this.d.locks.request(name, body);
      else await body();
    } catch (e) {
      // A closed generation must not keep issuing transactions; any error after stop is teardown.
      if (this.stopped) return;
      throw e;
    }
  }

  private async pushAll(): Promise<void> {
    const lib = this.d.libraryId;
    let sentAny = false;
    for (;;) {
      if (this.stopped) return;
      const rec = await this.d.store.replica(lib);
      if (this.stopped) return;
      if (!rec || rec.conflict) {
        await this.reload();
        return;
      }
      const rows = await this.d.store.outbox(lib, MAX_BATCH);
      if (this.stopped) return;
      if (rows.length === 0) break;
      const expected = toSeq20(fromSeq20(rec.acked_client_seq) + 1n);
      if (rows[0]!.client_seq !== expected) {
        await this.failClosed({
          client_seq: rows[0]!.client_seq,
          operation_id: rows[0]!.operation_id,
          status: 0,
          code: "outbox_gap",
          message: `outbox starts at ${rows[0]!.client_seq}, expected ${expected}`,
        });
        return;
      }
      const batch: PendingOp[] = [];
      let bytes = 0;
      for (const r of rows) {
        if (batch.length > 0 && bytes + r.envelope.length > MAX_BATCH_BYTES) break;
        batch.push(r);
        bytes += r.envelope.length;
      }
      this.set({ phase: "syncing" });
      try {
        const ack = await this.d.api.submit(lib, batch);
        await this.d.store.recordAcks(
          lib,
          batch.map((r, i) => ({ client_seq: r.client_seq, operation_id: r.operation_id, server_seq: ack.results[i]!.server_seq })),
        );
        this.attempt = 0;
        this.offline = false;
        sentAny = true;
        await this.reload();
        this.d.channel?.postMessage("changed");
      } catch (e) {
        if (e instanceof TransientError) {
          this.offline = true;
          await this.reload();
          this.set({ phase: "offline" });
          this.retryLater();
          return;
        }
        if (e instanceof ReplicaUnauthorizedError) {
          this.set({ phase: "locked" });
          this.d.onUnauthorized();
          return;
        }
        if (e instanceof RefusedError) {
          const i = e.envelopeIndex !== null && e.envelopeIndex < batch.length ? e.envelopeIndex : 0;
          await this.failClosed({
            client_seq: batch[i]!.client_seq,
            operation_id: batch[i]!.operation_id,
            status: e.status,
            code: e.code,
            message: e.message,
          });
          return;
        }
        // Storage failure while recording an ACK: the rows stay in the outbox; resending the
        // same bytes is safe (server dedup), so treat it as transient.
        this.retryLater();
        return;
      }
    }
    this.set({ phase: this.phaseFor(0, null) });
    if (sentAny) this.d.requestRefresh();
  }

  private retryLater(): void {
    if (this.stopped) return;
    const ms = backoffDelay(this.attempt++, this.random);
    this.timer = this.setT(() => {
      this.timer = null;
      this.kick();
    }, ms);
  }

  private async failClosed(c: ConflictInfo): Promise<void> {
    await this.d.store.setConflict(this.d.libraryId, c);
    await this.reload();
    this.d.channel?.postMessage("changed");
  }

  /** Clear a stored conflict and resubmit the preserved outbox (same envelope bytes). */
  async retryAfterConflict(): Promise<void> {
    await this.d.store.setConflict(this.d.libraryId, null);
    await this.reload();
    this.kick();
  }
}
