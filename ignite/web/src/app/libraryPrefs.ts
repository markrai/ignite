// Home/library sort preference and this browser's notebook-open recency (replica-local UI
// state; never notebook content, never a sync operation, never part of the snapshot).
//
// Both live in the UI preference store (`scribe-web-prefs`, see prefsDb.ts):
// * `librarySort` — "creation" | "alphabetical" | "recent";
// * `notebookAccess` — `{ next, byId }`: an ordinal access sequence, not wall-clock time.
//   Each notebook open takes `next + 1`; higher = more recently opened in this browser.
// Missing, older or malformed values read as Creation Order with no history. Records for
// notebooks that no longer exist are simply never looked up (notebook IDs are global UUIDs).
// Storage failures are harmless: the in-memory state keeps working for this page load.
//
// Consistency: one tab is the supported case. Each tab keeps its own in-memory `next` and
// rewrites the whole `notebookAccess` record, so concurrent tabs can mint the same ordinal and
// the last write wins (opens from the other tab are lost, never corrupted). This is NOT a
// strongly consistent profile-wide history; that would need a read-modify-write inside one
// IDB transaction (and e.g. BroadcastChannel to refresh other tabs).

import { SORT_MODES, type SortMode } from "../model/librarySort";
import { prefsStore, type PrefsStore } from "./prefsDb";

export const SORT_KEY = "librarySort";
export const ACCESS_KEY = "notebookAccess";

export interface LibraryPrefsState {
  sortMode: SortMode;
  /** notebook_id → access ordinal (higher = more recent). */
  access: ReadonlyMap<string, number>;
}

interface AccessRecord {
  next: number;
  byId: Record<string, number>;
}

function isOrdinal(v: unknown): v is number {
  return typeof v === "number" && Number.isSafeInteger(v) && v > 0;
}

export function parseSortMode(v: unknown): SortMode {
  return SORT_MODES.includes(v as SortMode) ? (v as SortMode) : "creation";
}

/** Tolerant read: anything malformed is dropped rather than failing the whole history. */
export function parseAccess(v: unknown): { next: number; access: Map<string, number> } {
  const access = new Map<string, number>();
  let next = 0;
  if (v && typeof v === "object") {
    const rec = v as Partial<AccessRecord>;
    if (rec.byId && typeof rec.byId === "object") {
      for (const [id, n] of Object.entries(rec.byId)) {
        if (isOrdinal(n)) {
          access.set(id, n);
          next = Math.max(next, n);
        }
      }
    }
    if (isOrdinal(rec.next)) next = Math.max(next, rec.next);
  }
  return { next, access };
}

export class LibraryPrefs {
  state: LibraryPrefsState = { sortMode: "creation", access: new Map() };
  private next = 0;
  private sortChosen = false;
  private listeners = new Set<() => void>();
  /** Loads first, then every save in call order, so writes never commit out of order. */
  private queue: Promise<void>;

  constructor(private readonly store: PrefsStore) {
    this.queue = this.load().catch(() => undefined);
  }

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  getState = (): LibraryPrefsState => this.state;

  /** Resolves once the stored preferences are loaded and every queued save has settled. */
  settled(): Promise<void> {
    return this.queue;
  }

  private emit(patch: Partial<LibraryPrefsState>): void {
    this.state = { ...this.state, ...patch };
    for (const l of this.listeners) l();
  }

  private async load(): Promise<void> {
    const [sort, rawAccess] = await Promise.all([this.store.get(SORT_KEY), this.store.get(ACCESS_KEY)]);
    const loaded = parseAccess(rawAccess);
    // Opens recorded before the load finished are newer than anything stored: renumber them
    // above the stored sequence, keeping their relative order.
    const early = [...this.state.access].sort((a, b) => a[1] - b[1]);
    let next = loaded.next;
    for (const [id] of early) loaded.access.set(id, ++next);
    this.next = next;
    // Restoring is silent (no toast) and never overrides a choice made during the load.
    // (Each early open already queued a save, which runs after this load.)
    this.emit({ sortMode: this.sortChosen ? this.state.sortMode : parseSortMode(sort), access: loaded.access });
  }

  private saveAccess(): Promise<void> {
    const byId: Record<string, number> = {};
    for (const [id, n] of this.state.access) byId[id] = n;
    const rec: AccessRecord = { next: this.next, byId };
    return this.store.put(ACCESS_KEY, rec);
  }

  private enqueue(save: () => Promise<void>): void {
    this.queue = this.queue.then(save).catch(() => undefined);
  }

  /** Advance to the next sort mode, persist it, and return it. */
  cycleSortMode(): SortMode {
    const i = SORT_MODES.indexOf(this.state.sortMode);
    const mode = SORT_MODES[(i + 1) % SORT_MODES.length]!;
    this.sortChosen = true;
    this.emit({ sortMode: mode });
    this.enqueue(() => this.store.put(SORT_KEY, mode));
    return mode;
  }

  /** Record that this browser opened `notebookId` (local UI state only). */
  recordAccess(notebookId: string): void {
    if (!notebookId) return;
    const access = new Map(this.state.access);
    access.set(notebookId, ++this.next);
    this.emit({ access });
    this.enqueue(() => this.saveAccess());
  }
}

let shared: LibraryPrefs | null = null;

/** The app's preference instance (created on first use, so tests never touch it). */
export function libraryPrefs(): LibraryPrefs {
  shared ??= new LibraryPrefs(prefsStore());
  return shared;
}

/** Default notebook-access observer for `LibrarySession`. */
export function recordNotebookAccess(notebookId: string): void {
  libraryPrefs().recordAccess(notebookId);
}
