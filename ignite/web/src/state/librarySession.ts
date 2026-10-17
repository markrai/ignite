// One open library: initial load, simple polling (no WebSockets), coherent swaps.
// (S3B: snapshot refresh is gated push-first by the sync coordinator; see `refreshGate`.)
//
// Poll every ~2.5 s: GET /v1/libraries/{id}. If head_server_seq and snapshot_sha256 are
// unchanged, nothing happens. Otherwise fetch the snapshot, validate it, preload the background
// the (preserved) selection needs, and only then swap — the previous valid page stays visible
// throughout, and a failed refresh leaves it in place with a stale indicator. Polling pauses
// while the document is hidden and runs immediately when it becomes visible again.

import { UnauthorizedError, type ReadOnlyApi } from "../api/readOnlyApi";
import { preserveSelection, resolve, type Selection } from "../model/selection";
import type { SnapshotResponse } from "../model/types";
import type { BlobCache } from "../rendering/background";

export const POLL_MS = 2500;

export interface SessionState {
  current: SnapshotResponse | null;
  selection: Selection | null;
  /** Initial load failed (nothing valid to show). */
  fatal: string | null;
  /** Last refresh failed; the previous valid state is still shown. */
  stale: string | null;
  /** Increments whenever something visible changes (snapshot, selection, background). */
  version: number;
}

export interface Clock {
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(h: unknown): void;
  hidden(): boolean;
  onVisibilityChange(fn: () => void): () => void;
}

export const browserClock: Clock = {
  setTimeout: (fn, ms) => globalThis.setTimeout(fn, ms),
  clearTimeout: (h) => globalThis.clearTimeout(h as ReturnType<typeof setTimeout>),
  hidden: () => typeof document !== "undefined" && document.hidden,
  onVisibilityChange: (fn) => {
    document.addEventListener("visibilitychange", fn);
    return () => document.removeEventListener("visibilitychange", fn);
  },
};

function message(e: unknown): string {
  return e instanceof Error ? e.message : "unexpected error";
}

export class LibrarySession {
  state: SessionState = { current: null, selection: null, fatal: null, stale: null, version: 0 };
  private listeners = new Set<() => void>();
  private timer: unknown = null;
  private inflight = false;
  private stopped = false;
  private unlisten: (() => void) | null = null;
  /**
   * S3B push-first gate: while it returns false (unACKed browser strokes exist) the displayed
   * snapshot is not replaced, so the view never regresses behind strokes the server may hold.
   */
  private refreshGate: (() => boolean) | null = null;
  /** Called after a snapshot is displayed (the coordinator compacts its accepted bridge). */
  private onSnapshot: ((through: bigint) => void) | null = null;

  setSyncHooks(refreshGate: (() => boolean) | null, onSnapshot: ((through: bigint) => void) | null): void {
    this.refreshGate = refreshGate;
    this.onSnapshot = onSnapshot;
  }

  constructor(
    private readonly api: ReadOnlyApi,
    readonly libraryId: string,
    private readonly blobs: BlobCache,
    private readonly onUnauthorized: () => void,
    private readonly clock: Clock = browserClock,
    /**
     * Notebook-open observer (see `select`), injected by the app shell; the session knows
     * nothing about where (or whether) opens are recorded. Never a sync operation.
     */
    private readonly onNotebookOpened: (notebookId: string) => void = () => undefined,
  ) {}

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private set(patch: Partial<SessionState>): void {
    this.state = { ...this.state, ...patch, version: this.state.version + 1 };
    for (const l of this.listeners) l();
  }

  /** Idempotent and restartable (React StrictMode mounts effects twice). */
  async start(): Promise<void> {
    this.stopped = false;
    this.unlisten?.();
    this.unlisten = this.clock.onVisibilityChange(() => {
      if (!this.clock.hidden()) void this.pollNow();
    });
    await this.pollNow();
  }

  stop(): void {
    this.stopped = true;
    if (this.timer !== null) this.clock.clearTimeout(this.timer);
    this.timer = null;
    this.unlisten?.();
    this.unlisten = null;
  }

  /**
   * The one user-navigation path (sidebar, page links, page steps, new notebook/page). A switch
   * to a different notebook is recorded as an open in this browser's local recency history;
   * page steps within the open notebook are not. Snapshot refreshes and the initial/restored
   * selection go through `refresh`, never here, so they never count as an open.
   */
  select(sel: Selection): void {
    const opened = sel.notebookId !== this.state.selection?.notebookId;
    this.set({ selection: sel });
    if (opened) this.onNotebookOpened(sel.notebookId);
    void this.ensureBackground();
  }

  /** Load the selected page's blob (if any) and repaint when it arrives. */
  async ensureBackground(): Promise<void> {
    const cur = this.state.current;
    if (!cur) return;
    const r = resolve(cur.snapshot, this.state.selection);
    if (!r || r.page.background.kind !== "blob" || this.blobs.get(r.page.background.sha256)) return;
    try {
      await this.blobs.load(r.page.background);
    } catch {
      // PageView shows the page-level background error from the cache miss + load failure.
    }
    if (!this.stopped) this.set({});
  }

  private schedule(): void {
    if (this.stopped) return;
    if (this.timer !== null) this.clock.clearTimeout(this.timer);
    this.timer = this.clock.setTimeout(() => {
      this.timer = null;
      if (this.clock.hidden()) {
        this.schedule(); // paused: check again later, resume at once on visibility
        return;
      }
      void this.pollNow();
    }, POLL_MS);
  }

  async pollNow(): Promise<void> {
    if (this.inflight || this.stopped) return;
    this.inflight = true;
    try {
      await this.refresh();
    } finally {
      this.inflight = false;
      this.schedule();
    }
  }

  private async refresh(): Promise<void> {
    try {
      const cur = this.state.current;
      if (cur && this.refreshGate && !this.refreshGate()) return;
      if (cur) {
        const head = await this.api.libraryHead(this.libraryId);
        if (head.snapshot_sha256 === cur.snapshot_sha256 && head.head_server_seq === cur.through_server_seq) {
          if (this.state.stale) this.set({ stale: null });
          return;
        }
      }
      const next = await this.api.snapshot(this.libraryId);
      if (this.stopped) return;
      const selection = preserveSelection(cur?.snapshot ?? null, this.state.selection, next.snapshot);
      const r = resolve(next.snapshot, selection);
      if (r && r.page.background.kind === "blob") await this.blobs.load(r.page.background);
      if (this.stopped) return;
      this.set({ current: next, selection, fatal: null, stale: null });
      this.onSnapshot?.(next.through_server_seq);
    } catch (e) {
      if (e instanceof UnauthorizedError) {
        this.onUnauthorized();
        return;
      }
      if (this.state.current) this.set({ stale: message(e) });
      else this.set({ fatal: message(e) });
    }
  }
}
