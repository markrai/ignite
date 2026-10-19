// Slice 1G-B: uploading a backup for verification and cancelling a prepared restore. Kept free
// of React (like RestoreJobsLoader) so the rules are testable on their own:
// * one upload at a time; a second click while one runs does nothing;
// * the selected File is sent as the request body (never read into memory);
// * success is shown only after a validated HTTP 201;
// * a failure after the request may have reached the server is reported as uncertain, never
//   as a refusal, and is never retried automatically;
// * the one-time confirmation from the 201 lives only in `pending` (volatile, never in `state`,
//   never rendered, stored or logged) and is dropped when the job is cancelled, expires or
//   becomes unusable, when access is lost, and on teardown. Confirming arrives in Slice 1G-C;
//   until then the capability is held but unused, and a page reload loses it by design.

import { ControlError, MAX_RESTORE_UPLOAD_BYTES, type ControlApi, type RestoreJob } from "../api/controlApi";
import { LOOPBACK_EXPLANATION, describeRestoreError, type RestoreErrorKind, type RestoreLoadError } from "./restoreJobsLoader";

export type ActionErrorKind =
  /** The request may have reached the server: it may have prepared (or cancelled) anyway. */
  | "uncertain"
  /** The server refused the backup itself (verification, type, size). */
  | "rejected"
  | "capacity"
  | "storage"
  | "session"
  | "loopback"
  | "access"
  | "server";

export interface ActionError {
  kind: ActionErrorKind;
  title: string;
  message: string;
  /** The server's code (and finer code, e.g. `archive_error`) when there is one. */
  code: string | null;
}

export type UploadState =
  | { kind: "idle" }
  | { kind: "selected"; name: string; size: number; warning: string | null }
  /** Refused locally (empty or over the server's ceiling): nothing was sent. */
  | { kind: "invalid"; name: string; size: number; message: string }
  | { kind: "uploading"; name: string; size: number }
  | { kind: "prepared"; name: string; job: RestoreJob }
  | { kind: "failed"; name: string; size: number; error: ActionError };

export interface CancelNotice {
  jobId: string;
  tone: "ok" | "warn" | "bad";
  text: string;
}

export interface CancelState {
  /** The job whose "discard?" question is open. */
  confirming: string | null;
  /** The job whose DELETE is in flight. */
  busy: string | null;
  notice: CancelNotice | null;
}

export interface ActionsState {
  upload: UploadState;
  cancel: CancelState;
  /** Bumped when an upload finishes, so the file input is reset (re-choosing the same file works). */
  picker: number;
}

export interface ActionHooks {
  onChange(s: ActionsState): void;
  /** A verified job arrived (show it in the list right away). */
  onPrepared(job: RestoreJob): void;
  /** Re-read the job list (after a cancel, an uncertain outcome or lost access). */
  refresh(): void;
  /** An action was refused for lack of access (401/403): drop privileged job data now. */
  accessRefused(error: RestoreLoadError): void;
}

function looksLikeZip(file: { name: string; type: string }): boolean {
  return /\.zip$/i.test(file.name) || /zip/i.test(file.type);
}

const INTERRUPTED_UPLOAD =
  "The connection ended before the server answered, so it is not known whether the backup was received. Refresh the list before uploading again: if a prepared restore appears, you can review or cancel it there. Its one-time confirmation cannot be recovered, so it would have to be uploaded again before it can be restored.";

export function describeUploadError(e: unknown): ActionError {
  if (!(e instanceof ControlError)) return { kind: "uncertain", title: "Upload outcome unknown", message: INTERRUPTED_UPLOAD, code: null };
  const code = e.detail ? `${e.code}: ${e.detail}` : e.code;
  if (e.status === 0 || e.code === "unsupported_response") {
    return { kind: "uncertain", title: "Upload outcome unknown", message: INTERRUPTED_UPLOAD, code: e.status === 0 ? null : code };
  }
  if (e.status === 401) return { kind: "session", title: "Not signed in", message: "Your administrator session is no longer valid. Sign in again before uploading.", code };
  if (e.status === 403 && e.code === "loopback_required") return { kind: "loopback", title: "Not available from this device", message: LOOPBACK_EXPLANATION, code };
  if (e.status === 403) return { kind: "access", title: "Upload refused", message: e.message, code };
  if (e.status === 413) {
    return { kind: "rejected", title: "Backup too large", message: "This backup exceeds the server's upload limit. Nothing was prepared.", code };
  }
  if (e.status === 422) return { kind: "rejected", title: "Backup could not be verified", message: `${e.message} Nothing was prepared.`, code };
  if (e.status === 400) return { kind: "rejected", title: "Upload refused", message: e.message, code };
  if (e.status === 429 && e.code === "upload_busy") {
    return { kind: "capacity", title: "Another upload is in progress", message: "The server verifies one backup at a time. Try again when the other upload has finished.", code };
  }
  if (e.status === 429) {
    return {
      kind: "capacity",
      title: "Too many prepared restores",
      message:
        "The server keeps at most one prepared restore per administrator (two in total). Cancel your prepared restore below, or wait for it to expire (15 minutes), then upload again.",
      code,
    };
  }
  if (e.status === 507) return { kind: "storage", title: "Not enough space on the server", message: "The server does not have enough free space to stage this backup. Nothing was prepared.", code };
  if (e.status === 408) return { kind: "server", title: "Upload too slow", message: `${e.message} Nothing was prepared.`, code };
  return { kind: "server", title: "Upload failed", message: e.message, code };
}

/** Errors that mean this browser no longer has (or never had) restore access. */
const ACCESS_LOST: readonly RestoreErrorKind[] = ["session", "loopback", "access"];

export class RestoreActions {
  private state: ActionsState = { upload: { kind: "idle" }, cancel: { confirming: null, busy: null, notice: null }, picker: 0 };
  private file: File | null = null;
  /** The one-time confirmation of the prepared job (never part of `state`). */
  private pending: { jobId: string; confirmation: string } | null = null;
  private expiry: ReturnType<typeof setTimeout> | null = null;
  private uploading = false;
  private abort: AbortController | null = null;
  private disposed = false;

  constructor(
    private readonly api: Pick<ControlApi, "uploadRestore" | "cancelRestore">,
    private readonly hooks: ActionHooks,
  ) {}

  get current(): ActionsState {
    return this.state;
  }

  /** Whether a confirmation is held for `jobId` (never the value itself). */
  holdsConfirmation(jobId: string): boolean {
    return this.pending?.jobId === jobId;
  }

  select(file: File | null): void {
    if (this.uploading) return;
    this.file = null;
    if (!file) return this.setUpload({ kind: "idle" });
    if (file.size === 0) return this.setUpload({ kind: "invalid", name: file.name, size: 0, message: "This file is empty. Choose an Ignite backup (.zip)." });
    if (file.size > MAX_RESTORE_UPLOAD_BYTES) {
      return this.setUpload({ kind: "invalid", name: file.name, size: file.size, message: "This file is larger than the 2 GB that Ignite Server accepts for a backup upload." });
    }
    this.file = file;
    this.setUpload({
      kind: "selected",
      name: file.name,
      size: file.size,
      warning: looksLikeZip(file) ? null : "This file does not look like a ZIP archive. The server will check it, but Ignite backups are .zip files.",
    });
  }

  async upload(): Promise<void> {
    const file = this.file;
    if (this.uploading || this.disposed || !file) return;
    this.uploading = true;
    this.abort = new AbortController();
    this.setUpload({ kind: "uploading", name: file.name, size: file.size });
    try {
      const { job, confirmation } = await this.api.uploadRestore(file, this.abort.signal);
      if (this.disposed) return;
      this.hold(job, confirmation);
      this.set({ ...this.state, upload: { kind: "prepared", name: file.name, job }, picker: this.state.picker + 1 });
      this.hooks.onPrepared(job);
    } catch (e) {
      if (this.disposed) return;
      const error = describeUploadError(e);
      this.set({ ...this.state, upload: { kind: "failed", name: file.name, size: file.size, error }, picker: this.state.picker + 1 });
      if (ACCESS_LOST.includes(error.kind as RestoreErrorKind)) this.refuseAccess(e);
      if (error.kind === "uncertain" || ACCESS_LOST.includes(error.kind as RestoreErrorKind)) this.hooks.refresh();
    } finally {
      // Settled (whatever the outcome): uploading again needs a new selection.
      if (this.file === file) this.file = null;
      this.uploading = false;
      this.abort = null;
    }
  }

  askCancel(jobId: string): void {
    if (this.state.cancel.busy) return;
    this.setCancel({ confirming: jobId, busy: null, notice: null });
  }

  keep(): void {
    if (this.state.cancel.busy) return;
    this.setCancel({ ...this.state.cancel, confirming: null });
  }

  async cancel(jobId: string): Promise<void> {
    if (this.state.cancel.busy || this.disposed) return;
    this.setCancel({ confirming: null, busy: jobId, notice: null });
    let notice: CancelNotice;
    try {
      await this.api.cancelRestore(jobId);
      this.drop(jobId);
      notice = { jobId, tone: "ok", text: "The prepared restore was discarded. Nothing was restored and no library changed." };
    } catch (e) {
      notice = this.cancelNotice(jobId, e);
    }
    if (this.disposed) return;
    if (this.state.upload.kind === "prepared" && this.state.upload.job.job_id === jobId && !this.pending) this.setUpload({ kind: "idle" });
    this.setCancel({ confirming: null, busy: null, notice });
    this.hooks.refresh();
  }

  private cancelNotice(jobId: string, e: unknown): CancelNotice {
    if (!(e instanceof ControlError) || e.status === 0) {
      return { jobId, tone: "warn", text: "The server did not answer, so it is not known whether the restore was cancelled. The list is being refreshed." };
    }
    if (e.status === 410) {
      this.drop(jobId);
      return { jobId, tone: "warn", text: "This prepared restore had already been cancelled or had expired." };
    }
    if (e.status === 404) {
      this.drop(jobId);
      return { jobId, tone: "warn", text: "This restore job no longer exists." };
    }
    if (e.status === 409) {
      this.drop(jobId); // executing: it can no longer be confirmed either
      return { jobId, tone: "bad", text: "This restore has started and can no longer be cancelled." };
    }
    if (e.code === "cleanup_failed") {
      this.drop(jobId);
      return { jobId, tone: "warn", text: "The prepared restore was withdrawn and can no longer be used. The server could not delete its staged files yet and keeps retrying." };
    }
    if (e.status === 401 || e.status === 403) {
      this.refuseAccess(e);
      return { jobId, tone: "bad", text: e.status === 403 && e.code === "loopback_required" ? LOOPBACK_EXPLANATION : "Cancelling was refused: your administrator access is no longer valid." };
    }
    return { jobId, tone: "bad", text: `Cancelling failed: ${e.message}` };
  }

  /**
   * Keep the held confirmation consistent with what the server shows: dropped when access is
   * lost, and when its job is gone or no longer usable (cancelled, expired, started).
   */
  reconcile(jobs: RestoreJob[] | null, error: RestoreErrorKind | null): void {
    if (!this.pending) return;
    if (error && ACCESS_LOST.includes(error)) return this.drop();
    if (!jobs) return;
    const held = this.pending.jobId;
    const job = jobs.find((j) => j.job_id === held);
    if (!job || !job.usable) {
      this.drop();
      if (this.state.upload.kind === "prepared" && this.state.upload.job.job_id === held) this.setUpload({ kind: "idle" });
    }
  }

  /** A definitive 401/403 on an action: forget the capability and drop the privileged list. */
  private refuseAccess(e: unknown): void {
    this.drop();
    this.hooks.accessRefused(describeRestoreError(e));
  }

  /** Teardown: stop an upload in flight (its outcome is then unknown) and forget the capability. */
  dispose(): void {
    this.disposed = true;
    this.abort?.abort();
    this.drop();
  }

  private hold(job: RestoreJob, confirmation: string): void {
    this.drop();
    this.pending = { jobId: job.job_id, confirmation };
    // Unusable once the prepared job expires (the server would refuse it anyway).
    this.expiry = setTimeout(
      () => {
        this.drop();
        if (!this.disposed) this.hooks.refresh();
      },
      Math.max(0, job.expires_in_s) * 1000,
    );
  }

  /** Forget the held confirmation (for `jobId` only, if given). */
  private drop(jobId?: string): void {
    if (jobId !== undefined && this.pending?.jobId !== jobId) return;
    this.pending = null;
    if (this.expiry) clearTimeout(this.expiry);
    this.expiry = null;
  }

  private setUpload(upload: UploadState): void {
    this.set({ ...this.state, upload });
  }

  private setCancel(cancel: CancelState): void {
    this.set({ ...this.state, cancel });
  }

  private set(s: ActionsState): void {
    if (this.disposed) return;
    this.state = s;
    this.hooks.onChange(s);
  }
}
