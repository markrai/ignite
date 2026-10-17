// Writable-replica startup: WASM bridge first, then IndexedDB.
//
// WASM is mandatory for authoring. Opening the replica store before the bridge
// succeeds leaves an ownerless IDB connection if compile is blocked (Safari CSP).

import type { ProtocolBridge } from "../wasm/bridge";

export interface CloseableStore {
  close(): void;
}

export interface WritableReplicaParts<S extends CloseableStore = CloseableStore> {
  bridge: ProtocolBridge;
  store: S;
}

/**
 * Load the protocol bridge, then open IndexedDB. Returns `null` if `live` became
 * false after a successful step (caller already torn down). Throws the original
 * bridge/storage error otherwise. A store opened after `live` flipped is closed
 * before returning.
 */
export async function startWritableReplica<S extends CloseableStore>(opts: {
  loadBridge: () => Promise<ProtocolBridge>;
  openStore: () => Promise<S>;
  live: () => boolean;
}): Promise<WritableReplicaParts<S> | null> {
  const bridge = await opts.loadBridge();
  if (!opts.live()) return null;
  const store = await opts.openStore();
  if (!opts.live()) {
    store.close();
    return null;
  }
  return { bridge, store };
}

function errorText(e: unknown): string {
  if (e instanceof Error) return e.message;
  return e != null ? String(e) : "";
}

function isWasmCspBlock(msg: string): boolean {
  return /WebAssembly/i.test(msg) && (/Content Security Policy/i.test(msg) || /unsafe-eval/i.test(msg));
}

function isStorageBlock(msg: string): boolean {
  return /indexeddb|idbdatabase|quotaexceeded|the database connection is closing/i.test(msg);
}

/**
 * User-visible replica-init failure. The raw exception stays in the console.
 * Pre-Safari-16 WebKit ignores `'wasm-unsafe-eval'` and reports this as
 * `'unsafe-eval'` not allowed on `script-src 'self' 'wasm-unsafe-eval'`.
 */
export function replicaUnavailableReason(e: unknown): string {
  const msg = errorText(e).trim();
  if (isWasmCspBlock(msg)) {
    return "Writing unavailable: WebAssembly is blocked by this browser's Content Security Policy.";
  }
  if (isStorageBlock(msg)) {
    return "Writing unavailable: browser storage could not be opened.";
  }
  if (msg) return `Writing unavailable: ${msg}`;
  return "Writing unavailable: this browser could not start a writable replica.";
}
