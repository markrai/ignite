// Slice 1G-D2: the administrator's backup exports (create, follow, download, discard). Kept free
// of React, like the restore loader and actions, so the rules are testable on their own. It
// shares nothing with the restore state machines.
// * never two list requests at once (a refresh while one is in flight joins it), and nothing is
//   applied after `dispose`;
// * a list requested before newer data arrived (a 202, a dismissal) is discarded, and a
//   "preparing" answer never replaces a ready, failed or withdrawn one for the same export (the
//   server never goes back to preparing);
// * a failed list keeps the last one (marked with the error); a refused session or access
//   drops it at once, and nothing started before that refusal (a POST, a DELETE, a list) may
//   bring export data back: only a list requested afterwards can;
// * a backup the server definitively withdrew (DELETE answered with deferred cleanup, or
//   `cleanup_failed`) loses its Download at once and is shown as cleanup pending;
// * the list is re-read only while an export is preparing, and when a ready export's
//   server-reported time runs out (the server decides whether it expired);
// * Create sends one POST per explicit click and is never repeated automatically. A POST without
//   an answer is "outcome unknown" until a list fetched afterwards shows what the server has;
// * a download is a native browser download of a same-origin link: nothing here reads the
//   archive, and starting one proves nothing about the saved file.

import { ControlError, exportDownloadPath, type BackupExport, type ControlApi } from "../api/controlApi";

/** While an export is being prepared, the list is re-read this often. */
export const PREPARING_POLL_MS = 2000;

export const BACKUP_LOOPBACK_EXPLANATION =
  "Backup administration is only available when Ignite is opened on the server itself (its local loopback address) or through a secure tunnel to it, such as SSH port forwarding. Access from other devices on the network is refused by design.";

export type ExportLoadErrorKind =
  /** 401: not (or no longer) signed in. */
  | "session"
  /** 403 `loopback_required`. */
  | "loopback"
  /** Any other access refusal (403). */
  | "access"
  /** The server answered something this version does not understand. */
  | "unsupported"
  /** Unreachable or a server error: the last list stays visible. */
  | "transient";

export interface ExportLoadError {
  kind: ExportLoadErrorKind;
  message: string;
}

export interface ExportActionError {
  kind: "capacity" | "storage" | "session" | "loopback" | "access" | "csrf" | "server";
  title: string;
  message: string;
  code: string | null;
}

export type CreateState =
  | { kind: "idle" }
  | { kind: "sending" }
  /** HTTP 202: the server accepted the request and is preparing (not ready). */
  | { kind: "accepted"; exportId: string }
  /** No usable answer: the server may have started an export. `checkFailed`: the list could not be read. */
  | { kind: "uncertain"; checkFailed: boolean }
  /** After an unknown outcome, a fresh list shows an export (it may be the requested one). */
  | { kind: "uncertain_found" }
  /** After an unknown outcome, a fresh list shows no new export. */
  | { kind: "uncertain_none" }
  | { kind: "refused"; error: ExportActionError };

export interface DismissNotice {
  exportId: string;
  tone: "ok" | "warn" | "bad";
  text: string;
}

export interface DismissState {
  /** The export whose "discard?" question is open. */
  confirming: string | null;
  /** The export whose DELETE is in flight. */
  busy: string | null;
  notice: DismissNotice | null;
}

export interface BackupExportsState {
  /** Newest first; `null` until the first successful list, and again after access was refused. */
  exports: BackupExport[] | null;
  loading: boolean;
  error: ExportLoadError | null;
  create: CreateState;
  dismiss: DismissState;
  /** Client time (`Date.now()`) by which each ready export's reported time runs out. */
  deadlines: Readonly<Record<string, number>>;
  /** A download was handed to the browser for this export (started, not confirmed). */
  downloadStarted: string | null;
}

export const INITIAL_EXPORTS_STATE: BackupExportsState = {
  exports: null,
  loading: false,
  error: null,
  create: { kind: "idle" },
  dismiss: { confirming: null, busy: null, notice: null },
  deadlines: {},
  downloadStarted: null,
};

export function describeExportLoadError(e: unknown): ExportLoadError {
  if (!(e instanceof ControlError)) return { kind: "transient", message: "Could not load backups." };
  if (e.code === "unsupported_response") return { kind: "unsupported", message: e.message };
  if (e.status === 401) return { kind: "session", message: "Your administrator session is no longer valid. Sign in again to manage backups." };
  if (e.status === 403 && e.code === "loopback_required") return { kind: "loopback", message: BACKUP_LOOPBACK_EXPLANATION };
  if (e.status === 403 && e.code !== "csrf") return { kind: "access", message: e.message || "Backup administration was refused." };
  return { kind: "transient", message: e.message || "Could not load backups." };
}

export function describeCreateError(e: unknown): ExportActionError | "uncertain" {
  if (!(e instanceof ControlError) || e.status === 0) return "uncertain"; // no answer, or not understood
  const code = e.code;
  if (e.status === 401) return { kind: "session", title: "Not signed in", message: "Your administrator session is no longer valid. No backup was started.", code };
  if (e.status === 403 && code === "loopback_required") return { kind: "loopback", title: "Not available from this device", message: BACKUP_LOOPBACK_EXPLANATION, code };
  if (e.status === 403 && code === "csrf") {
    return { kind: "csrf", title: "Request refused", message: "The server refused the request as a possible cross-site request. No backup was started. Reload the page and try again.", code };
  }
  if (e.status === 403 && code === "browser_session_required") {
    return { kind: "access", title: "Browser session required", message: "Backups can only be created from an administrator's browser session. No backup was started.", code };
  }
  if (e.status === 403) return { kind: "access", title: "Backup refused", message: e.message, code };
  if (e.status === 429 && code === "export_exists") {
    return { kind: "capacity", title: "You already have a backup", message: "You already have an export. Download or dismiss it before creating another.", code };
  }
  if (e.status === 429 && code === "export_busy") {
    return { kind: "capacity", title: "Server busy", message: "Another backup is currently being prepared. Try again when it has finished.", code };
  }
  if (e.status === 429) {
    return { kind: "capacity", title: "Export capacity reached", message: "The server has reached its export capacity. Try again after other backups are downloaded, discarded or expire.", code };
  }
  if (e.status === 507) {
    return { kind: "storage", title: "Not enough space on the server", message: "The server does not have enough free space to prepare a backup. No backup was started.", code };
  }
  if (e.status === 503) return { kind: "server", title: "Server shutting down", message: "The server is shutting down and did not start a backup.", code };
  if (e.status === 500) return { kind: "server", title: "Backup could not be started", message: e.message, code };
  return { kind: "server", title: "Backup could not be started", message: e.message, code };
}

/** States that hold a backup on the server for this administrator (Create would be refused). */
const HOLDS: readonly string[] = ["preparing", "ready", "expired", "cleanup_pending"];

/** Newest first (export ids are time-ordered UUIDv7 text). */
const newestFirst = (list: BackupExport[]) => [...list].sort((a, b) => (a.export_id < b.export_id ? 1 : a.export_id > b.export_id ? -1 : 0));

/** Can a new export be requested now (as far as this page knows; the server stays authoritative)? */
export function canCreate(s: BackupExportsState): boolean {
  if (!s.exports || (s.error && s.error.kind !== "transient")) return false;
  if (s.create.kind === "sending" || s.create.kind === "uncertain") return false;
  return !s.exports.some((e) => HOLDS.includes(e.state));
}

/** The validated download link of a ready export whose time has not run out, else `null`. */
export function downloadHref(e: BackupExport, deadline: number | undefined, now: number): string | null {
  if (e.state !== "ready" || deadline === undefined || now >= deadline) return null;
  const expected = exportDownloadPath(e.export_id);
  return e.download === expected ? expected : null;
}

export class BackupExportsController {
  private state: BackupExportsState = INITIAL_EXPORTS_STATE;
  private inFlight: Promise<void> | null = null;
  private disposed = false;
  /** List requests are numbered; answers to requests up to `staleThrough` are discarded. */
  private seq = 0;
  private staleThrough = 0;
  /** Successful, non-stale lists applied (evidence of what the server reported). */
  private serverLists = 0;
  /** An unanswered POST is settled only by a list with a higher version than this. */
  private uncertainAt = Number.MAX_SAFE_INTEGER;
  /** Export ids shown when the unanswered POST was sent. */
  private knownBefore: ReadonlySet<string> = new Set();
  private creating = false;
  /**
   * Authorization epoch: advanced by every access refusal. An operation started in an earlier
   * epoch may not change export data or start anything when it settles.
   */
  private epoch = 0;
  private poll: ReturnType<typeof setTimeout> | null = null;
  private expiry: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private readonly api: Pick<ControlApi, "listBackupExports" | "startBackupExport" | "dismissBackupExport">,
    private readonly onChange: (s: BackupExportsState) => void,
    private readonly now: () => number = () => Date.now(),
  ) {}

  get current(): BackupExportsState {
    return this.state;
  }

  get preparing(): boolean {
    return this.state.exports?.some((e) => e.state === "preparing") ?? false;
  }

  refresh(): Promise<void> {
    if (this.disposed) return Promise.resolve();
    if (this.inFlight) return this.inFlight;
    this.clearPoll();
    this.set({ ...this.state, loading: true });
    const mine = ++this.seq;
    const stale = () => mine <= this.staleThrough;
    const run = this.api.listBackupExports().then(
      (list) => {
        if (stale()) return this.set({ ...this.state, loading: false });
        this.serverLists++;
        this.apply(list);
      },
      (e: unknown) => {
        if (stale()) return this.set({ ...this.state, loading: false });
        // Shown (a kept list is marked with it), but no evidence of the server's state.
        const error = describeExportLoadError(e);
        if (error.kind !== "transient") return this.refuse(error);
        const create = this.state.create.kind === "uncertain" ? { kind: "uncertain" as const, checkFailed: true } : this.state.create;
        this.set({ ...this.state, loading: false, error, create });
      },
    );
    this.inFlight = run.finally(() => {
      this.inFlight = null;
      this.schedule();
    });
    return this.inFlight;
  }

  /** A fresh list, not overtaken by a request already in flight. */
  refreshAfter(): Promise<void> {
    return this.inFlight ? this.inFlight.then(() => this.refresh()) : this.refresh();
  }

  /** The administrator's explicit request for a new backup: at most one POST per click. */
  async create(): Promise<void> {
    if (this.creating || this.disposed || !canCreate(this.state)) return;
    this.creating = true;
    const epoch = this.epoch;
    this.knownBefore = new Set((this.state.exports ?? []).map((e) => e.export_id));
    this.set({ ...this.state, create: { kind: "sending" }, dismiss: { ...this.state.dismiss, notice: null }, downloadStarted: null });
    try {
      const accepted = await this.api.startBackupExport();
      if (this.disposed) return;
      // Access was refused meanwhile: this answer may not bring export data back.
      if (epoch !== this.epoch) return this.set({ ...this.state, create: { kind: "idle" } });
      this.upsert(accepted);
      this.set({ ...this.state, create: { kind: "accepted", exportId: accepted.export_id } });
      void this.refreshAfter();
    } catch (e) {
      if (this.disposed) return;
      if (epoch !== this.epoch) return this.set({ ...this.state, create: { kind: "idle" } });
      const error = describeCreateError(e);
      if (error === "uncertain") {
        // The server may have accepted it: find out from a list requested from now on. Never resent.
        this.staleThrough = this.seq;
        this.uncertainAt = this.serverLists;
        this.set({ ...this.state, create: { kind: "uncertain", checkFailed: false } });
        void this.refreshAfter();
      } else if (error.kind === "session" || error.kind === "loopback" || error.kind === "access") {
        this.refuse(describeExportLoadError(e));
        this.set({ ...this.state, create: { kind: "refused", error } });
      } else {
        this.set({ ...this.state, create: { kind: "refused", error } });
        if (error.kind === "capacity") void this.refreshAfter(); // show what the server holds
      }
    } finally {
      this.creating = false;
    }
  }

  askDismiss(exportId: string): void {
    const e = this.state.exports?.find((x) => x.export_id === exportId);
    if (!e || e.state === "preparing" || e.state === "cleanup_pending" || this.state.dismiss.busy) return;
    this.setDismiss({ confirming: exportId, busy: null, notice: null });
  }

  keep(): void {
    if (this.state.dismiss.busy) return;
    this.setDismiss({ ...this.state.dismiss, confirming: null });
  }

  /** Discard a ready, expired or failed export's temporary files (after the inline question). */
  async dismiss(exportId: string): Promise<void> {
    const d = this.state.dismiss;
    if (this.disposed || d.busy || d.confirming !== exportId) return;
    const e = this.state.exports?.find((x) => x.export_id === exportId);
    if (!e || e.state === "preparing") return;
    this.setDismiss({ confirming: null, busy: exportId, notice: null });
    const epoch = this.epoch;
    let notice: DismissNotice;
    try {
      const r = await this.api.dismissBackupExport(exportId);
      if (this.disposed) return;
      if (epoch !== this.epoch) return this.setDismiss({ confirming: null, busy: null, notice: null });
      // Withdrawn on the server: lists requested before now are outdated.
      this.staleThrough = this.seq;
      notice =
        r.cleanup === "deferred"
          ? {
              exportId,
              tone: "warn",
              text: "The backup was withdrawn and can no longer be downloaded. A download already in progress may finish; the server deletes the temporary file afterwards.",
            }
          : { exportId, tone: "ok", text: "The temporary backup was removed from the server. Your notebooks and any ZIP you saved are not affected." };
      if (r.cleanup === "done") this.forget(exportId);
      else this.withdraw(exportId);
    } catch (err) {
      if (this.disposed) return;
      if (epoch !== this.epoch) return this.setDismiss({ confirming: null, busy: null, notice: null });
      notice = this.dismissNotice(exportId, err);
    }
    this.setDismiss({ confirming: null, busy: null, notice });
    if (this.state.downloadStarted === exportId) this.set({ ...this.state, downloadStarted: null });
    void this.refreshAfter();
  }

  private dismissNotice(exportId: string, e: unknown): DismissNotice {
    if (!(e instanceof ControlError) || e.status === 0) {
      return { exportId, tone: "warn", text: "The server did not answer, so it is not known whether the backup was discarded. The list is being refreshed." };
    }
    if (e.status === 409) return { exportId, tone: "warn", text: "This backup is still being prepared and cannot be discarded yet. Discard it once it is ready." };
    if (e.status === 404) return { exportId, tone: "warn", text: "The server no longer has this backup." };
    if (e.status === 410) return { exportId, tone: "warn", text: "This backup had already expired." };
    if (e.code === "cleanup_failed") {
      // Definitively withdrawn; only its file remains (deleted by the server's retries).
      this.staleThrough = this.seq;
      this.withdraw(exportId);
      return { exportId, tone: "warn", text: "The backup was withdrawn and can no longer be downloaded. The server could not delete its temporary file yet and retries automatically." };
    }
    if (e.status === 401 || (e.status === 403 && e.code !== "csrf")) {
      this.refuse(describeExportLoadError(e));
      return { exportId, tone: "bad", text: "Discarding was refused: your administrator access is no longer valid." };
    }
    return { exportId, tone: "bad", text: `Discarding failed: ${e.message}` };
  }

  /**
   * The browser was handed this export's download link (a native download began, or was
   * refused). Nothing is known about the saved file; the list is re-read in case the export
   * is no longer available.
   */
  downloadStarted(exportId: string): void {
    if (this.disposed) return;
    this.set({ ...this.state, downloadStarted: exportId });
    void this.refreshAfter();
  }

  dispose(): void {
    this.disposed = true;
    this.clearPoll();
    if (this.expiry) clearTimeout(this.expiry);
    this.expiry = null;
  }

  /** Apply a successful, current list. */
  private apply(list: BackupExport[]): void {
    const previous = new Map((this.state.exports ?? []).map((e) => [e.export_id, e]));
    const received = this.now();
    const exports = newestFirst(
      list.map((e) => {
        const p = previous.get(e.export_id);
        return p && p.state !== "preparing" && e.state === "preparing" ? p : e;
      }),
    );
    const deadlines: Record<string, number> = {};
    for (const e of exports) {
      if (e.state !== "ready" || e.expires_in_s === null) continue;
      const fresh = list.find((x) => x.export_id === e.export_id) === e;
      deadlines[e.export_id] = fresh ? received + e.expires_in_s * 1000 : (this.state.deadlines[e.export_id] ?? received);
    }
    let create = this.state.create;
    if (create.kind === "uncertain" && this.serverLists > this.uncertainAt) {
      const found = exports.some((e) => !this.knownBefore.has(e.export_id) || e.state === "preparing" || e.state === "ready");
      create = found ? { kind: "uncertain_found" } : { kind: "uncertain_none" };
      this.uncertainAt = Number.MAX_SAFE_INTEGER;
    }
    if (create.kind === "accepted") {
      const id = create.exportId;
      if (!exports.some((e) => e.export_id === id)) create = { kind: "idle" };
    }
    const ids = new Set(exports.map((e) => e.export_id));
    const confirming = this.state.dismiss.confirming;
    const dismiss = confirming && !ids.has(confirming) ? { ...this.state.dismiss, confirming: null } : this.state.dismiss;
    const downloadStarted = this.state.downloadStarted && ids.has(this.state.downloadStarted) ? this.state.downloadStarted : null;
    this.set({ ...this.state, exports, loading: false, error: null, create, dismiss, deadlines, downloadStarted });
    this.scheduleExpiry();
  }

  /** Show an export the server just returned (a 202) before the next list arrives. */
  private upsert(e: BackupExport): void {
    this.staleThrough = this.seq;
    const existing = this.state.exports?.find((x) => x.export_id === e.export_id);
    if (existing && existing.state !== "preparing") return; // already further along
    const others = (this.state.exports ?? []).filter((x) => x.export_id !== e.export_id);
    this.set({ ...this.state, exports: newestFirst([e, ...others]), error: null });
  }

  /** The server deleted this export: it is no longer listed. */
  private forget(exportId: string): void {
    if (!this.state.exports) return;
    const deadlines = { ...this.state.deadlines };
    delete deadlines[exportId];
    this.set({ ...this.state, exports: this.state.exports.filter((x) => x.export_id !== exportId), deadlines });
  }

  /**
   * The server withdrew this export but its file still awaits deletion: shown as cleanup
   * pending, never downloadable again (a later successful list stays authoritative).
   */
  private withdraw(exportId: string): void {
    if (!this.state.exports) return;
    const deadlines = { ...this.state.deadlines };
    delete deadlines[exportId];
    const exports = this.state.exports.map((x) => (x.export_id === exportId ? { ...x, state: "cleanup_pending" as const, download: null } : x));
    this.set({ ...this.state, exports, deadlines });
  }

  /** Access was refused: drop privileged data now and discard every request in flight. */
  private refuse(error: ExportLoadError): void {
    this.epoch++;
    this.staleThrough = this.seq;
    this.clearPoll();
    this.set({
      ...this.state,
      exports: null,
      loading: false,
      error,
      deadlines: {},
      downloadStarted: null,
      dismiss: { confirming: null, busy: this.state.dismiss.busy, notice: null },
      create: this.state.create.kind === "uncertain" ? { kind: "idle" } : this.state.create,
    });
  }

  /** Re-read the list while an export is preparing (one timer, never overlapping a request). */
  private schedule(): void {
    this.clearPoll();
    if (this.disposed || this.inFlight || !this.preparing || (this.state.error && this.state.error.kind !== "transient")) return;
    this.poll = setTimeout(() => {
      this.poll = null;
      void this.refresh();
    }, PREPARING_POLL_MS);
  }

  /** When a ready export's reported time runs out, ask the server (it decides). */
  private scheduleExpiry(): void {
    if (this.expiry) clearTimeout(this.expiry);
    this.expiry = null;
    const times = Object.values(this.state.deadlines);
    if (this.disposed || times.length === 0) return;
    const wait = Math.max(0, Math.min(...times) - this.now()) + 1000;
    this.expiry = setTimeout(() => {
      this.expiry = null;
      void this.refreshAfter();
    }, wait);
  }

  private clearPoll(): void {
    if (this.poll) clearTimeout(this.poll);
    this.poll = null;
  }

  private setDismiss(dismiss: DismissState): void {
    this.set({ ...this.state, dismiss });
  }

  private set(s: BackupExportsState): void {
    if (this.disposed) return;
    this.state = s;
    this.onChange(s);
  }
}
