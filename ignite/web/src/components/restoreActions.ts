// Slices 1G-B/1G-C: uploading a backup for verification, cancelling a prepared restore, and
// confirming it. Kept free of React (like RestoreJobsLoader) so the rules are testable alone:
// * one upload at a time; a second click while one runs does nothing;
// * the selected File is sent as the request body (never read into memory);
// * success is shown only after a validated HTTP 201;
// * a failure after a request may have reached the server is reported as uncertain, never as a
//   refusal, and nothing is ever retried automatically;
// * the one-time confirmation from the 201 lives only in `pending` (volatile, never in `state`,
//   never rendered, stored or logged), together with the immutable plan it approves (archive
//   SHA-256 and exact mapping, from the same 201). The confirmation request is built only from
//   that plan, only after an explicit acknowledged final action, and only while the latest
//   server summary still shows the same ready, usable job with the same archive and mapping;
// * it is dropped when the job is cancelled, expires, becomes unusable or differs, when access
//   is lost, when the server accepts or definitively refuses it, and on teardown. A page reload
//   loses it by design (the job can then only be reviewed or cancelled).

import { ControlError, MAX_RESTORE_UPLOAD_BYTES, type ControlApi, type RestoreConfirmation, type RestoreJob } from "../api/controlApi";
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

export type ConfirmState =
  | { kind: "idle" }
  /** The final review is open; nothing has been sent. */
  | { kind: "reviewing"; jobId: string; acknowledged: boolean }
  | { kind: "sending"; jobId: string }
  /** HTTP 202: the server started the restore (not a result). */
  | { kind: "accepted"; jobId: string }
  /** No answer: the server may or may not have started it; its status is being checked. */
  | { kind: "uncertain"; jobId: string }
  /** After an unanswered confirmation, the server still shows the job ready: nothing started. */
  | { kind: "not_started"; jobId: string }
  /** The confirmed job is no longer on the server (its result was not kept). */
  | { kind: "gone"; jobId: string }
  | { kind: "refused"; jobId: string; error: ActionError };

export interface ActionsState {
  upload: UploadState;
  cancel: CancelState;
  confirm: ConfirmState;
  /**
   * The job this browser can confirm (it holds the matching capability and the latest server
   * summary shows the approved plan, ready and usable). Never the capability itself.
   */
  confirmable: string | null;
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
  /**
   * The server's answer to a confirmation changed (or may have changed) the job: discard every
   * list requested before now, fetch a fresh one, and return the list version that is now
   * outdated (only later versions may settle the confirmation).
   */
  recheck(): number;
}

/** The plan a held confirmation approves, captured from the validated 201 (never mutated). */
interface HeldPlan {
  readonly jobId: string;
  readonly confirmation: string;
  readonly archiveSha256: string;
  /** Source → destination, as the server proposed them. */
  readonly mapping: ReadonlyArray<readonly [string, string]>;
  /** `Date.now()` after which the prepared job has expired. */
  readonly deadline: number;
}

function planOf(job: RestoreJob, confirmation: string): HeldPlan {
  return Object.freeze({
    jobId: job.job_id,
    confirmation,
    archiveSha256: job.archive.sha256,
    mapping: Object.freeze(job.libraries.map((l) => Object.freeze([l.source_library_id, l.destination_library_id] as const))),
    deadline: Date.now() + Math.max(0, job.expires_in_s) * 1000,
  });
}

const pairKey = (source: string, destination: string) => `${source}\u0000${destination}`;

/** The server's summary still describes exactly the held plan (mapping order irrelevant). */
function matchesPlan(plan: HeldPlan, job: RestoreJob): boolean {
  if (job.job_id !== plan.jobId || job.archive.sha256 !== plan.archiveSha256) return false;
  const shown = new Set(job.libraries.map((l) => pairKey(l.source_library_id, l.destination_library_id)));
  const held = new Set(plan.mapping.map(([s, d]) => pairKey(s, d)));
  return shown.size === job.libraries.length && shown.size === held.size && [...held].every((k) => shown.has(k));
}

function describeConfirmError(e: unknown): ActionError {
  if (!(e instanceof ControlError) || e.status === 0 || e.code === "unsupported_response") {
    return {
      kind: "uncertain",
      title: "Confirmation outcome unknown",
      message: "Checking restore status. The server may already have started this restore, so it is not sent again.",
      code: null,
    };
  }
  const code = e.code;
  if (e.status === 401) return { kind: "session", title: "Not signed in", message: "Your administrator session is no longer valid. Nothing was restored.", code };
  if (e.status === 403 && e.code === "confirmation_mismatch") {
    return {
      kind: "rejected",
      title: "Confirmation refused",
      message: "The server did not accept this confirmation for the prepared restore. Nothing was restored. Upload the backup again to restore it.",
      code,
    };
  }
  if (e.status === 403 && e.code === "loopback_required") return { kind: "loopback", title: "Not available from this device", message: LOOPBACK_EXPLANATION, code };
  if (e.status === 403) return { kind: "access", title: "Confirmation refused", message: e.message, code };
  if (e.status === 429) {
    return { kind: "capacity", title: "Another restore is running", message: "The server restores one backup at a time. Nothing was started; confirm again when the other restore has finished.", code };
  }
  if (e.status === 409) return { kind: "server", title: "Already confirmed", message: "This restore was already confirmed. Its status is shown below.", code };
  if (e.status === 410) return { kind: "server", title: "No longer available", message: `${e.message} Nothing was restored.`, code };
  if (e.status === 404) return { kind: "server", title: "Restore job not found", message: "This restore job no longer exists. Nothing was restored by this confirmation.", code };
  if (e.status === 503) return { kind: "server", title: "Server shutting down", message: "The server is shutting down and did not start this restore.", code };
  if (e.status === 500) return { kind: "server", title: "Restore could not be started", message: `${e.message} Its status is shown below.`, code };
  return { kind: "server", title: "Confirmation failed", message: e.message, code };
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
  private state: ActionsState = {
    upload: { kind: "idle" },
    cancel: { confirming: null, busy: null, notice: null },
    confirm: { kind: "idle" },
    confirmable: null,
    picker: 0,
  };
  private file: File | null = null;
  /** The one-time confirmation and the plan it approves (never part of `state`). */
  private pending: HeldPlan | null = null;
  private expiry: ReturnType<typeof setTimeout> | null = null;
  private uploading = false;
  private confirming = false;
  /** The server version (successful lists only) that predates the last confirmation answer. */
  private answeredAt = 0;
  private abort: AbortController | null = null;
  private disposed = false;

  constructor(
    private readonly api: Pick<ControlApi, "uploadRestore" | "cancelRestore" | "confirmRestore">,
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
    if (this.state.cancel.busy || this.confirmLocks(jobId)) return;
    this.setCancel({ confirming: jobId, busy: null, notice: null });
  }

  /** A confirmation for `jobId` is open, in flight, accepted or unresolved: no cancelling. */
  private confirmLocks(jobId: string): boolean {
    const c = this.state.confirm;
    return this.confirming || (c.kind !== "idle" && c.kind !== "refused" && c.kind !== "not_started" && c.jobId === jobId);
  }

  keep(): void {
    if (this.state.cancel.busy) return;
    this.setCancel({ ...this.state.cancel, confirming: null });
  }

  async cancel(jobId: string): Promise<void> {
    if (this.state.cancel.busy || this.disposed || this.confirmLocks(jobId)) return;
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
  reconcile(jobs: RestoreJob[] | null, error: RestoreErrorKind | null, version: number = Number.MAX_SAFE_INTEGER): void {
    if (error && ACCESS_LOST.includes(error)) {
      this.drop();
      return;
    }
    if (!jobs) return;
    // Only a successful server list answered after the last confirmation answer may settle
    // what it did (`version` counts successful server lists only; a failed GET never does).
    if (version > this.answeredAt) this.settleConfirmation(jobs);
    if (!this.pending) return;
    const plan = this.pending;
    const job = jobs.find((j) => j.job_id === plan.jobId);
    const usable = !!job && job.state === "ready" && job.usable;
    if (!job || !usable || !matchesPlan(plan, job)) {
      const differs = !!job && usable;
      this.drop();
      if (this.state.upload.kind === "prepared" && this.state.upload.job.job_id === plan.jobId) this.setUpload({ kind: "idle" });
      const c = this.state.confirm;
      if (c.kind === "reviewing" && c.jobId === plan.jobId) {
        this.setConfirm({
          kind: "refused",
          jobId: plan.jobId,
          error: {
            kind: "server",
            title: "Can no longer be confirmed",
            message: differs
              ? "The server now describes this prepared restore differently from the reviewed preview, so it cannot be confirmed. Upload the backup again."
              : "This prepared restore is no longer ready (it expired, was cancelled or was started). Nothing was sent.",
            code: null,
          },
        });
      }
      return;
    }
    if (this.state.confirmable !== plan.jobId) this.set({ ...this.state, confirmable: plan.jobId });
  }

  /** After an accepted or unanswered confirmation, follow what a fresh list shows. */
  private settleConfirmation(jobs: RestoreJob[]): void {
    const c = this.state.confirm;
    if (c.kind !== "accepted" && c.kind !== "uncertain") return;
    const job = jobs.find((j) => j.job_id === c.jobId);
    const stillReady = !!job && job.state === "ready" && job.usable;
    // Once it started (or vanished), the upload preview ("nothing restored") is no longer true.
    if (!stillReady && this.state.upload.kind === "prepared" && this.state.upload.job.job_id === c.jobId) this.setUpload({ kind: "idle" });
    if (!job) return this.setConfirm({ kind: "gone", jobId: c.jobId });
    if (job.state === "executing") {
      this.drop(c.jobId);
      if (c.kind === "uncertain") this.setConfirm({ kind: "accepted", jobId: c.jobId });
      return;
    }
    if (c.kind === "uncertain" && stillReady) {
      // Not started. Another attempt needs a new, explicit decision (and a still-valid plan).
      return this.setConfirm({ kind: "not_started", jobId: c.jobId });
    }
    // Finished (or withdrawn): the row shows what the server reports.
    this.drop(c.jobId);
    this.setConfirm({ kind: "idle" });
  }

  /** Open the final review (no request). Only for the job this browser can confirm. */
  review(jobId: string): void {
    if (this.disposed || this.confirming || this.state.cancel.busy) return;
    if (this.state.confirmable !== jobId || this.pending?.jobId !== jobId) return;
    const c = this.state.confirm;
    if (c.kind !== "idle" && c.kind !== "refused" && c.kind !== "not_started") return;
    this.set({ ...this.state, confirm: { kind: "reviewing", jobId, acknowledged: false }, cancel: { ...this.state.cancel, confirming: null } });
  }

  acknowledge(jobId: string, acknowledged: boolean): void {
    const c = this.state.confirm;
    if (c.kind === "reviewing" && c.jobId === jobId) this.setConfirm({ kind: "reviewing", jobId, acknowledged });
  }

  /** Leave the final review without sending anything. */
  back(): void {
    if (this.state.confirm.kind === "reviewing") this.setConfirm({ kind: "idle" });
  }

  /**
   * The administrator's final, acknowledged action: send the held confirmation once, built
   * from the held plan only. Never repeated automatically, whatever happens.
   */
  async confirm(jobId: string): Promise<void> {
    const c = this.state.confirm;
    if (this.confirming || this.disposed || this.state.cancel.busy) return;
    if (c.kind !== "reviewing" || c.jobId !== jobId || !c.acknowledged) return;
    const plan = this.pending;
    if (!plan || plan.jobId !== jobId || this.state.confirmable !== jobId) return;
    if (Date.now() >= plan.deadline) {
      this.drop(jobId);
      this.setConfirm({ kind: "refused", jobId, error: { kind: "server", title: "Expired", message: "This prepared restore expired before it was confirmed. Nothing was sent.", code: null } });
      this.hooks.refresh();
      return;
    }
    this.confirming = true;
    this.setConfirm({ kind: "sending", jobId });
    const body: RestoreConfirmation = {
      v: 1,
      confirmation: plan.confirmation,
      archive_sha256: plan.archiveSha256,
      destinations: plan.mapping.map(([source_library_id, destination_library_id]) => ({ source_library_id, destination_library_id })),
    };
    try {
      await this.api.confirmRestore(jobId, body);
      if (this.disposed) return;
      this.drop(jobId); // consumed by the server
      if (this.state.upload.kind === "prepared" && this.state.upload.job.job_id === jobId) this.setUpload({ kind: "idle" });
      this.setConfirm({ kind: "accepted", jobId });
      this.answered();
    } catch (e) {
      if (this.disposed) return;
      const error = describeConfirmError(e);
      if (error.kind === "uncertain") {
        // Kept, but locked: nothing more is sent until a fresh list settles what happened.
        this.setConfirm({ kind: "uncertain", jobId });
        this.answered();
      } else if (error.kind === "capacity") {
        this.setConfirm({ kind: "refused", jobId, error }); // still ready; still confirmable later
      } else {
        if (ACCESS_LOST.includes(error.kind as RestoreErrorKind)) this.refuseAccess(e);
        else this.drop(jobId);
        this.setConfirm({ kind: "refused", jobId, error });
        if (!ACCESS_LOST.includes(error.kind as RestoreErrorKind)) this.answered();
      }
    } finally {
      this.confirming = false;
    }
  }

  /** The server answered (or may have): only lists fetched from now on may settle it. */
  private answered(): void {
    // Nothing may settle while the re-check starts (it can report the old list synchronously);
    // then only server versions after the one it outdated count.
    this.answeredAt = Number.MAX_SAFE_INTEGER;
    this.answeredAt = this.hooks.recheck();
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
    const plan = planOf(job, confirmation);
    this.pending = plan;
    this.set({ ...this.state, confirmable: job.job_id });
    // Unusable once the prepared job expires (the server would refuse it anyway).
    this.expiry = setTimeout(
      () => {
        if (this.pending !== plan) return;
        this.drop();
        const c = this.state.confirm;
        if (c.kind === "reviewing" && c.jobId === plan.jobId) {
          this.setConfirm({ kind: "refused", jobId: plan.jobId, error: { kind: "server", title: "Expired", message: "This prepared restore expired before it was confirmed. Nothing was sent.", code: null } });
        }
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
    if (this.state.confirmable !== null) this.set({ ...this.state, confirmable: null });
  }

  private setConfirm(confirm: ConfirmState): void {
    this.set({ ...this.state, confirm });
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
