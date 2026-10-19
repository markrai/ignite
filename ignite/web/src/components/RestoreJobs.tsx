// Server Settings → Backup & Restore. 1G-A lists this administrator's restore jobs; 1G-B adds
// uploading a backup for verification (a preview) and cancelling a prepared restore; 1G-C adds
// the explicit, acknowledged confirmation and follows the server's execution and result. Every
// server string is rendered as text. The one-time confirmation from an upload never reaches this
// view: it stays inside RestoreActions (the view only learns which job is confirmable).

import { useEffect, useRef, useState, type ReactNode } from "react";
import type { ControlApi, RestoreJob, RestoreJobState } from "../api/controlApi";
import { RestoreActions, type ActionsState, type ConfirmState, type UploadState } from "./restoreActions";
import { RestoreJobsLoader, type RestoreJobsState } from "./restoreJobsLoader";

/** While a restore runs, its job is re-read this often (nothing is polled otherwise). */
const EXECUTING_POLL_MS = 2000;

const STATE_LABEL: Record<RestoreJobState, string> = {
  ready: "Verified, awaiting confirmation",
  executing: "Restoration in progress",
  succeeded: "Restoration completed",
  failed: "Restoration failed",
  expired: "Expired",
  cleanup_pending: "Withdrawn, removing staged files",
};

/** A failed job whose COMMIT outcome is unknown may have published: never labelled a plain failure. */
function stateLabel(job: RestoreJob): { label: string; tone: string } {
  if (job.state === "failed" && job.result?.outcome === "failed" && job.result.publication === "unknown") {
    return { label: "Restore outcome uncertain", tone: "uncertain" };
  }
  return { label: STATE_LABEL[job.state], tone: job.state };
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

function minutes(s: number): string {
  const m = Math.max(1, Math.ceil(s / 60));
  return m === 1 ? "1 minute" : `${m} minutes`;
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

const RETENTION = "The server keeps this result for a limited time and not after it restarts.";

/**
 * A confirmation of this job is being sent, was accepted, or got no answer: a cached "ready" row
 * is then no longer a statement of the current status (the server may already be restoring it,
 * or be done) until a fresh server list arrives. The row's `state` stays the server's.
 */
function checkingStatus(confirm: ConfirmState, job: RestoreJob): boolean {
  return job.state === "ready" && (confirm.kind === "sending" || confirm.kind === "accepted" || confirm.kind === "uncertain") && confirm.jobId === job.job_id;
}

/**
 * The upload preview's "no notebooks have been restored" is true only while nothing may have
 * been started for its job: before confirming, during the review, after a fresh list showed it
 * not started, or after a refusal that started nothing (429).
 */
function previewStillTrue(confirm: ConfirmState, jobId: string): boolean {
  if (confirm.kind === "idle" || confirm.jobId !== jobId) return true;
  if (confirm.kind === "reviewing" || confirm.kind === "not_started") return true;
  return confirm.kind === "refused" && confirm.error.kind === "capacity";
}

function Outcome({ job, confirmable, checking }: { job: RestoreJob; confirmable: boolean; checking: boolean }) {
  const r = job.result;
  if (checking) {
    return (
      <p className="hint" data-testid="restore-ready-note">
        Checking the server&apos;s status for this restore. The last list showed it ready, but it may already be running or finished.
      </p>
    );
  }
  if (job.state === "ready" && job.usable && confirmable) {
    return (
      <p className="hint" data-testid="restore-ready-note">
        Verified and ready. Review it, then choose Restore libraries… to create new libraries from it. This prepared restore expires in {minutes(job.expires_in_s)}.
      </p>
    );
  }
  if (job.state === "ready") {
    return (
      <p className="hint" data-testid="restore-ready-note">
        This backup has been verified, but this browser no longer has its one-time confirmation. To restore it, discard this prepared job and upload the backup again.
      </p>
    );
  }
  if (job.state === "executing") return <p className="hint">The server is restoring this backup. This list refreshes automatically.</p>;
  if (job.state === "expired") return <p className="hint">This prepared restore expired before it was confirmed. Nothing was restored.</p>;
  if (job.state === "cleanup_pending") {
    return <p className="hint">This prepared restore was cancelled or expired. Nothing was restored; the server is still removing its staged files.</p>;
  }
  if (!r) return <p className="hint">The result of this restore is no longer available.</p>;
  if (r.outcome === "succeeded") {
    return (
      <div className="notice ok" data-testid="restore-publication" data-publication="complete">
        {r.report.libraries.length === 1 ? "1 library restored as a new library." : `${r.report.libraries.length} libraries restored as new libraries.`}{" "}
        {plural(r.report.blobs_installed, "background file")} added,{" "}
        {r.report.blobs_already_present} already present. Existing libraries were not replaced.
        {job.cleanup_pending && <span className="hint block">The restore is complete; the server is still removing its staged files.</span>}
        <span className="hint block">{RETENTION}</span>
      </div>
    );
  }
  return (
    <div className={`notice ${r.publication === "unknown" ? "warn" : "bad"}`} data-testid="restore-publication" data-publication={r.publication}>
      <div>
        <span className="mono">{r.error}</span>: {r.message}
      </div>
      {r.publication === "none" ? (
        <div>No libraries were published by this restore.</div>
      ) : (
        <div>
          <strong>Publication outcome could not be confirmed.</strong> Check the destination libraries before attempting another restore; a new attempt may create
          duplicate libraries under different identities.
        </div>
      )}
      {r.unindexed_blob_files_possible && (
        <span className="hint block">Verified background files may remain on the server without being used. They are harmless and are reused by a later restore.</span>
      )}
      {job.cleanup_pending && <span className="hint block">The server is still removing this restore&apos;s staged files.</span>}
      <span className="hint block">{RETENTION}</span>
    </div>
  );
}

/** The final review: what will happen, an explicit acknowledgement, then the one action that sends. */
function ConfirmPanel({
  job,
  confirm,
  onAcknowledge,
  onConfirm,
  onBack,
}: {
  job: RestoreJob;
  confirm: Extract<ConfirmState, { kind: "reviewing" | "sending" }>;
  onAcknowledge: (value: boolean) => void;
  onConfirm: () => void;
  onBack: () => void;
}) {
  const sending = confirm.kind === "sending";
  const acknowledged = confirm.kind === "reviewing" && confirm.acknowledged;
  const n = job.totals.libraries;
  const headingId = `restore-confirm-heading-${job.job_id}`;
  return (
    <div className="restore-confirm" role="group" aria-labelledby={headingId} data-testid="restore-confirm-panel">
      <h3 id={headingId}>Confirm restoration</h3>
      <div>This backup contains:</div>
      <ul className="restore-not-restored">
        <li>{plural(n, "library", "libraries")}</li>
        <li>{plural(job.totals.notebooks, "notebook")}</li>
        <li>{plural(job.totals.pages, "page")}</li>
        <li>{plural(job.totals.blobs, "background file")}</li>
      </ul>
      <div>
        Ignite will create <strong>{n === 1 ? "1 new library" : `${n} new libraries`}</strong>. Your existing libraries will not be replaced, merged or deleted.
      </div>
      <Details job={job} id={`restore-confirm-details-${job.job_id}`} />
      <label className="restore-ack">
        <input type="checkbox" checked={acknowledged || sending} disabled={sending} onChange={(e) => onAcknowledge(e.currentTarget.checked)} data-testid="restore-ack" />
        <span>I understand that this will create new libraries and may result in duplicate content if I have already restored this backup.</span>
      </label>
      <div className="row-actions">
        <button className="btn primary" onClick={onConfirm} disabled={!acknowledged || sending} aria-busy={sending} data-testid="restore-confirm-yes">
          {sending ? "Sending confirmation…" : "Restore new libraries"}
        </button>
        <button className="btn" onClick={onBack} disabled={sending} data-testid="restore-confirm-back">
          Go back
        </button>
      </div>
      {sending && (
        <p className="hint" role="status">
          Sending confirmation…
        </p>
      )}
    </div>
  );
}

/** What the last confirmation of `confirm.jobId` led to (not shown once the row shows a result). */
function ConfirmNotice({ confirm, job }: { confirm: ConfirmState; job: RestoreJob | null }) {
  if (confirm.kind === "accepted") {
    if (job && job.state !== "ready" && job.state !== "executing") return null; // the result is shown
    return (
      <div role="status" className="notice ok" data-testid="restore-confirm-notice" data-kind="accepted">
        Restoration accepted. The server is restoring the verified backup.
      </div>
    );
  }
  if (confirm.kind === "uncertain") {
    return (
      <div role="status" className="notice warn" data-testid="restore-confirm-notice" data-kind="uncertain">
        <strong>Confirmation outcome unknown. Checking restore status.</strong>
        <div>The server may already have started this restore, so the confirmation is not sent again. Refresh to check its status.</div>
      </div>
    );
  }
  if (confirm.kind === "not_started") {
    return (
      <div role="status" className="notice warn" data-testid="restore-confirm-notice" data-kind="not_started">
        The server has not started this restore and nothing was restored. To restore it, review it and confirm again.
      </div>
    );
  }
  if (confirm.kind === "gone") {
    return (
      <div role="status" className="notice warn" data-testid="restore-confirm-notice" data-kind="gone">
        The server no longer has this restore job or its result ({RETENTION.toLowerCase().replace(/\.$/, "")}). Check the server&apos;s libraries to see whether it was
        restored before restoring the backup again.
      </div>
    );
  }
  if (confirm.kind === "refused") {
    return (
      <div role="alert" className={`notice ${confirm.error.kind === "capacity" ? "warn" : "bad"}`} data-testid="restore-confirm-notice" data-kind={`refused-${confirm.error.kind}`}>
        <strong>{confirm.error.title}</strong>
        <div>{confirm.error.message}</div>
        {confirm.error.code && <span className="hint block mono">{confirm.error.code}</span>}
      </div>
    );
  }
  return null;
}

/** Upload a backup for server-side verification (a preview; nothing is restored). */
function UploadPanel({
  upload,
  picker,
  confirm,
  onSelect,
  onUpload,
}: {
  upload: UploadState;
  picker: number;
  confirm: ConfirmState;
  onSelect: (f: File | null) => void;
  onUpload: () => void;
}) {
  const busy = upload.kind === "uploading";
  return (
    <div className="restore-upload" data-testid="restore-upload" data-upload={upload.kind}>
      <h3>Upload backup</h3>
      <label className="restore-file">
        <span className="hint block">Ignite backup file (.zip)</span>
        <input
          key={picker}
          type="file"
          accept=".zip,application/zip"
          disabled={busy}
          onChange={(e) => onSelect(e.currentTarget.files?.[0] ?? null)}
          data-testid="restore-file"
          aria-describedby="restore-upload-status"
        />
      </label>
      {(upload.kind === "selected" || upload.kind === "invalid" || upload.kind === "uploading" || upload.kind === "failed") && (
        <div className="hint" data-testid="restore-file-info">
          <span className="mono">{upload.name}</span> · {formatBytes(upload.size)}
        </div>
      )}
      {upload.kind === "selected" && upload.warning && <div className="notice warn">{upload.warning}</div>}
      {upload.kind === "invalid" && (
        <div role="alert" className="notice bad" data-testid="restore-file-invalid">
          {upload.message}
        </div>
      )}
      <div className="row-actions">
        <button className="btn primary" onClick={onUpload} disabled={upload.kind !== "selected"} aria-busy={busy} data-testid="restore-upload-button">
          {busy ? "Uploading and verifying…" : "Upload and verify"}
        </button>
      </div>
      <div id="restore-upload-status" role="status" aria-live="polite">
        {busy && (
          <p className="hint" data-testid="restore-uploading">
            Uploading and verifying the backup. A large backup can take several minutes; keep this page open.
          </p>
        )}
        {upload.kind === "prepared" && previewStillTrue(confirm, upload.job.job_id) && (
          <div className="notice ok" data-testid="restore-prepared">
            <strong>Verification complete</strong>
            <div>
              {plural(upload.job.totals.libraries, "library", "libraries")} · {plural(upload.job.totals.notebooks, "notebook")} · {plural(upload.job.totals.pages, "page")} ·{" "}
              {plural(upload.job.totals.blobs, "background file")} ({formatBytes(upload.job.totals.blob_bytes)})
            </div>
            <div>
              <strong>This is a preview. No notebooks have been restored.</strong> Review the libraries below, then choose Restore libraries… to restore them as new
              libraries.
            </div>
            <span className="hint block">
              The prepared restore expires in {minutes(upload.job.expires_in_s)}. If this page is reloaded, its one-time confirmation is lost: it can still be reviewed or
              cancelled, but the backup would have to be uploaded again before it can be restored.
            </span>
          </div>
        )}
        {upload.kind === "failed" && (
          <div
            role="alert"
            className={`notice ${upload.error.kind === "uncertain" || upload.error.kind === "capacity" ? "warn" : "bad"}`}
            data-testid="restore-upload-error"
            data-kind={upload.error.kind}
          >
            <strong>{upload.error.title}</strong>
            <div>{upload.error.message}</div>
            {upload.error.code && <span className="hint block mono">{upload.error.code}</span>}
          </div>
        )}
      </div>
    </div>
  );
}

function Details({ job, id }: { job: RestoreJob; id: string }) {
  const restored = job.result?.outcome === "succeeded" ? job.result.report.libraries : null;
  const destination = (source: string, proposed: string) => restored?.find((l) => l.source_library_id === source)?.library_id ?? proposed;
  const destinationLabel = restored ? "Restored as" : job.result?.publication === "unknown" ? "Proposed new library (check whether it exists)" : "Proposed new library";
  return (
    <div id={id} className="restore-details" data-testid="restore-details">
      <dl className="kv">
        <dt>Job</dt>
        <dd className="mono">{job.job_id}</dd>
        <dt>Archive SHA-256</dt>
        <dd className="mono">{job.archive.sha256}</dd>
        <dt>Format</dt>
        <dd>
          {job.archive.format} v{job.archive.format_version}
        </dd>
        <dt>Restores</dt>
        <dd>Current notebook state only</dd>
        <dt>Background files</dt>
        <dd>
          {job.totals.blobs} ({formatBytes(job.totals.blob_bytes)})
        </dd>
      </dl>
      {job.not_restored.length > 0 && (
        <div className="hint">
          Not included in a backup:
          <ul className="restore-not-restored" data-testid="restore-not-restored">
            {job.not_restored.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        </div>
      )}
      {job.warnings.length > 0 && (
        <div className="hint">
          Notes from verification:
          <ul className="restore-not-restored" data-testid="restore-warnings">
            {job.warnings.map((w, i) => (
              <li key={i}>{w}</li>
            ))}
          </ul>
        </div>
      )}
      <ul className="rows restore-libraries">
        {job.libraries.map((l) => (
          <li key={l.source_library_id} className="restore-library" data-testid="restore-library">
            <dl className="kv">
              <dt>Library in backup</dt>
              <dd className="mono" data-testid="restore-source">
                {l.source_library_id}
              </dd>
              <dt>{destinationLabel}</dt>
              <dd className="mono" data-testid="restore-destination">
                {destination(l.source_library_id, l.destination_library_id)}
              </dd>
              <dt>Contents</dt>
              <dd>
                {plural(l.notebook_count, "notebook")}, {plural(l.page_count, "page")}
              </dd>
              {restored?.some((r) => r.source_library_id === l.source_library_id) && (
                <>
                  <dt>Restored snapshot SHA-256</dt>
                  <dd className="mono" data-testid="restore-restored-sha">
                    {restored.find((r) => r.source_library_id === l.source_library_id)!.restored_snapshot_sha256}
                  </dd>
                </>
              )}
            </dl>
          </li>
        ))}
      </ul>
    </div>
  );
}

const IDLE_ACTIONS: ActionsState = {
  upload: { kind: "idle" },
  cancel: { confirming: null, busy: null, notice: null },
  confirm: { kind: "idle" },
  confirmable: null,
  picker: 0,
};

export interface RestoreActionHandlers {
  onSelect: (file: File | null) => void;
  onUpload: () => void;
  onAskCancel: (jobId: string) => void;
  onKeep: () => void;
  onCancel: (jobId: string) => void;
  onReview: (jobId: string) => void;
  onAcknowledge: (jobId: string, value: boolean) => void;
  onBack: () => void;
  onConfirm: (jobId: string) => void;
}

/** A confirmation for this job is open, in flight, accepted or unresolved (no other action). */
function confirmLocks(confirm: ConfirmState, jobId: string): boolean {
  return (confirm.kind === "reviewing" || confirm.kind === "sending" || confirm.kind === "accepted" || confirm.kind === "uncertain") && confirm.jobId === jobId;
}

export function RestoreJobsView({
  state,
  expanded,
  onToggle,
  onRefresh,
  actions = IDLE_ACTIONS,
  handlers,
  backup,
}: {
  state: RestoreJobsState;
  expanded: ReadonlySet<string>;
  onToggle: (jobId: string) => void;
  onRefresh: () => void;
  /** Upload and cancel state (never the one-time confirmation). Omitted: read-only. */
  actions?: ActionsState;
  handlers?: RestoreActionHandlers;
  /** The Backup half of the section (1G-D2), shown above Restore. */
  backup?: ReactNode;
}) {
  const { jobs, loading, error } = state;
  const { cancel, confirm } = actions;
  const confirmJobShown = confirm.kind !== "idle" && confirm.kind !== "reviewing" && confirm.kind !== "sending" && !!jobs?.some((j) => j.job_id === confirm.jobId);
  // Uploading needs restore access: hidden while it is refused (the server would refuse anyway).
  const accessRefused = error !== null && error.kind !== "transient";
  // With the Backup half above it, Refresh belongs to the Restore subsection.
  const refresh = (
    <button className="btn" onClick={onRefresh} disabled={loading} aria-label={backup ? "Refresh restores" : undefined} data-testid="restore-refresh">
      {loading ? "Refreshing…" : "Refresh"}
    </button>
  );
  return (
    <section className="settings-card" data-testid="restore-card" aria-labelledby="restore-heading">
      <div className="card-head">
        <h2 id="restore-heading">Backup &amp; Restore</h2>
        {!backup && refresh}
      </div>
      <p className="hint">
        An Ignite backup preserves each library&apos;s current notebooks and pages (not its edit history). Restoring always creates new libraries; existing libraries are never
        replaced or merged.
      </p>
      {backup}
      {backup && (
        <div className="card-head restore-subhead">
          <h3>Restore</h3>
          {refresh}
        </div>
      )}
      {error && (
        <div role="alert" className={error.kind === "transient" ? "error-text" : "notice warn"} data-testid="restore-error" data-kind={error.kind}>
          {error.message}
          {error.kind === "transient" && jobs && <span className="hint block">Showing the last list that loaded.</span>}
        </div>
      )}
      {handlers && !accessRefused && <UploadPanel upload={actions.upload} picker={actions.picker} confirm={confirm} onSelect={handlers.onSelect} onUpload={handlers.onUpload} />}
      {handlers && confirm.kind !== "idle" && !confirmJobShown && confirm.kind !== "reviewing" && confirm.kind !== "sending" && (
        <ConfirmNotice confirm={confirm} job={null} />
      )}
      {cancel.notice && (
        <div role="status" className={`notice ${cancel.notice.tone}`} data-testid="restore-cancel-notice" data-tone={cancel.notice.tone}>
          {cancel.notice.text}
        </div>
      )}
      {!jobs && !error && (
        <p className="hint" data-testid="restore-loading">
          Loading restore jobs…
        </p>
      )}
      {jobs && jobs.length === 0 && (
        <div data-testid="restore-empty">
          <p>No restore jobs yet.</p>
          <p className="hint">Upload an Ignite backup to verify it and preview what it would restore.</p>
        </div>
      )}
      {jobs && jobs.length > 0 && (
        <ul className="rows">
          {jobs.map((job) => {
            const open = expanded.has(job.job_id);
            const checking = checkingStatus(confirm, job);
            const status = checking
              ? { label: confirm.kind === "sending" ? "Sending confirmation" : "Checking server status", tone: "executing" }
              : stateLabel(job);
            const detailsId = `restore-details-${job.job_id}`;
            return (
              <li key={job.job_id} className="restore-job" data-testid="restore-job" data-state={job.state} data-job-id={job.job_id}>
                <div className="row">
                  <div>
                    <div className="row-title">
                      <span className={`status status-${status.tone}`} data-testid="restore-state">
                        {status.label}
                      </span>
                      {job.cleanup_pending && job.state !== "cleanup_pending" && <span className="tag">cleanup pending</span>}
                    </div>
                    <div className="hint">
                      {formatBytes(job.archive.bytes)} backup · {plural(job.totals.libraries, "library", "libraries")} · {plural(job.totals.notebooks, "notebook")} ·{" "}
                      {plural(job.totals.pages, "page")}
                      {job.state === "ready" && !checking && <> · {job.usable ? `usable, expires in ${minutes(job.expires_in_s)}` : "no longer usable"}</>}
                    </div>
                  </div>
                  <div className="row-actions">
                    <button className="btn subtle" aria-expanded={open} aria-controls={detailsId} onClick={() => onToggle(job.job_id)} data-testid="restore-toggle">
                      {open ? "Hide details" : "Details"}
                    </button>
                    {handlers &&
                      actions.confirmable === job.job_id &&
                      job.state === "ready" &&
                      job.usable &&
                      !confirmLocks(confirm, job.job_id) &&
                      cancel.confirming !== job.job_id &&
                      cancel.busy !== job.job_id && (
                        <button className="btn primary" onClick={() => handlers.onReview(job.job_id)} disabled={cancel.busy !== null} data-testid="restore-review">
                          Restore libraries…
                        </button>
                      )}
                    {handlers && job.state === "ready" && job.usable && cancel.confirming !== job.job_id && !confirmLocks(confirm, job.job_id) && (
                      <button className="btn" onClick={() => handlers.onAskCancel(job.job_id)} disabled={cancel.busy !== null} data-testid="restore-cancel">
                        {cancel.busy === job.job_id ? "Cancelling…" : "Cancel…"}
                      </button>
                    )}
                  </div>
                </div>
                {handlers && cancel.confirming === job.job_id && (
                  <div className="notice warn" role="group" aria-label="Discard this prepared restore?" data-testid="restore-cancel-confirm">
                    <div>Discard this prepared restore? Nothing has been restored and no library will change. To restore this backup later, upload it again.</div>
                    <div className="row-actions">
                      <button className="btn primary" onClick={() => handlers.onCancel(job.job_id)} disabled={cancel.busy !== null} data-testid="restore-cancel-yes">
                        Discard prepared restore
                      </button>
                      <button className="btn" onClick={handlers.onKeep} disabled={cancel.busy !== null} data-testid="restore-cancel-no">
                        Keep
                      </button>
                    </div>
                  </div>
                )}
                {handlers && (confirm.kind === "reviewing" || confirm.kind === "sending") && confirm.jobId === job.job_id && (
                  <ConfirmPanel
                    job={job}
                    confirm={confirm}
                    onAcknowledge={(v) => handlers.onAcknowledge(job.job_id, v)}
                    onConfirm={() => handlers.onConfirm(job.job_id)}
                    onBack={handlers.onBack}
                  />
                )}
                {handlers && confirm.kind !== "idle" && confirm.kind !== "reviewing" && confirm.kind !== "sending" && confirm.jobId === job.job_id && (
                  <ConfirmNotice confirm={confirm} job={job} />
                )}
                <Outcome job={job} confirmable={actions.confirmable === job.job_id} checking={checking} />
                {open && !((confirm.kind === "reviewing" || confirm.kind === "sending") && confirm.jobId === job.job_id) && <Details job={job} id={detailsId} />}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

/** The Backup & Restore section (its own loading and refresh, independent of the rest). */
export function RestoreJobs({ api, backup }: { api: ControlApi; backup?: ReactNode }) {
  const [state, setState] = useState<RestoreJobsState>({ jobs: null, loading: false, error: null });
  const [actionsState, setActionsState] = useState<ActionsState>(IDLE_ACTIONS);
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const loader = useRef<RestoreJobsLoader | null>(null);
  // Holds the one-time confirmation of a fresh upload (inside RestoreActions, never in state).
  const actions = useRef<RestoreActions | null>(null);

  useEffect(() => {
    const l = new RestoreJobsLoader(() => api.restoreJobs(), setState);
    loader.current = l;
    const a = new RestoreActions(api, {
      onChange: setActionsState,
      onPrepared: (job) => {
        l.upsert(job);
        setExpanded((s) => new Set(s).add(job.job_id));
        void l.refreshAfter();
      },
      refresh: () => void l.refreshAfter(),
      accessRefused: (e) => l.refuse(e),
      recheck: () => {
        const outdated = l.invalidate();
        void l.refreshAfter();
        return outdated;
      },
    });
    actions.current = a;
    const first = setTimeout(() => void l.refresh(), 0);
    return () => {
      clearTimeout(first);
      l.dispose();
      a.dispose(); // aborts an upload in flight and forgets the confirmation
      if (loader.current === l) loader.current = null;
      if (actions.current === a) actions.current = null;
    };
  }, [api]);

  // The held confirmation follows what the server shows (cancelled, expired, access lost).
  useEffect(() => {
    actions.current?.reconcile(state.jobs, state.error?.kind ?? null, loader.current?.serverVersion ?? 0);
  }, [state]);

  // Only while a restore runs: re-read the list until it finishes.
  const executing = state.jobs?.some((j) => j.state === "executing") ?? false;
  useEffect(() => {
    if (!executing) return;
    const t = setInterval(() => void loader.current?.refresh(), EXECUTING_POLL_MS);
    return () => clearInterval(t);
  }, [executing]);

  const toggle = (id: string) =>
    setExpanded((s) => {
      const next = new Set(s);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  const handlers: RestoreActionHandlers = {
    onSelect: (f) => actions.current?.select(f),
    onUpload: () => void actions.current?.upload(),
    onAskCancel: (id) => actions.current?.askCancel(id),
    onKeep: () => actions.current?.keep(),
    onCancel: (id) => void actions.current?.cancel(id),
    onReview: (id) => actions.current?.review(id),
    onAcknowledge: (id, v) => actions.current?.acknowledge(id, v),
    onBack: () => actions.current?.back(),
    onConfirm: (id) => void actions.current?.confirm(id),
  };
  return (
    <RestoreJobsView
      state={state}
      expanded={expanded}
      onToggle={toggle}
      onRefresh={() => void loader.current?.refresh()}
      actions={actionsState}
      handlers={handlers}
      backup={backup}
    />
  );
}
