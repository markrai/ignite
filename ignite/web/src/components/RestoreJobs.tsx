// Server Settings → Backup & Restore. 1G-A lists this administrator's restore jobs; 1G-B adds
// uploading a backup for verification (a preview: nothing is restored) and cancelling a prepared
// restore. Confirming a restore comes in a later slice, so no control for it is shown. Every
// server string is rendered as text. The one-time confirmation from an upload never reaches this
// view: it stays inside RestoreActions.

import { useEffect, useRef, useState } from "react";
import type { ControlApi, RestoreJob, RestoreJobState } from "../api/controlApi";
import { RestoreActions, type ActionsState, type UploadState } from "./restoreActions";
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

function Outcome({ job }: { job: RestoreJob }) {
  const r = job.result;
  if (job.state === "ready") {
    return <p className="hint">Confirming a restore is not yet available in the browser. This prepared restore expires in {minutes(job.expires_in_s)}.</p>;
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
        {r.report.blobs_already_present} already present.
        {job.cleanup_pending && <span className="hint block">The restore is complete; the server is still removing its staged files.</span>}
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
    </div>
  );
}

/** Upload a backup for server-side verification (a preview; nothing is restored). */
function UploadPanel({ upload, picker, onSelect, onUpload }: { upload: UploadState; picker: number; onSelect: (f: File | null) => void; onUpload: () => void }) {
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
        {upload.kind === "prepared" && (
          <div className="notice ok" data-testid="restore-prepared">
            <strong>Verification complete</strong>
            <div>
              {plural(upload.job.totals.libraries, "library", "libraries")} · {plural(upload.job.totals.notebooks, "notebook")} · {plural(upload.job.totals.pages, "page")} ·{" "}
              {plural(upload.job.totals.blobs, "background file")} ({formatBytes(upload.job.totals.blob_bytes)})
            </div>
            <div>
              <strong>This is a preview. No notebooks have been restored.</strong> Review the libraries below. Confirming a restore will be available in a later version.
            </div>
            <span className="hint block">
              The prepared restore expires in {minutes(upload.job.expires_in_s)}. If this page is reloaded it can still be reviewed or cancelled, but the backup would have
              to be uploaded again before it can be restored.
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
            </dl>
          </li>
        ))}
      </ul>
    </div>
  );
}

const IDLE_ACTIONS: ActionsState = { upload: { kind: "idle" }, cancel: { confirming: null, busy: null, notice: null }, picker: 0 };

export interface RestoreActionHandlers {
  onSelect: (file: File | null) => void;
  onUpload: () => void;
  onAskCancel: (jobId: string) => void;
  onKeep: () => void;
  onCancel: (jobId: string) => void;
}

export function RestoreJobsView({
  state,
  expanded,
  onToggle,
  onRefresh,
  actions = IDLE_ACTIONS,
  handlers,
}: {
  state: RestoreJobsState;
  expanded: ReadonlySet<string>;
  onToggle: (jobId: string) => void;
  onRefresh: () => void;
  /** Upload and cancel state (never the one-time confirmation). Omitted: read-only. */
  actions?: ActionsState;
  handlers?: RestoreActionHandlers;
}) {
  const { jobs, loading, error } = state;
  const { cancel } = actions;
  // Uploading needs restore access: hidden while it is refused (the server would refuse anyway).
  const accessRefused = error !== null && error.kind !== "transient";
  return (
    <section className="settings-card" data-testid="restore-card" aria-labelledby="restore-heading">
      <div className="card-head">
        <h2 id="restore-heading">Backup &amp; Restore</h2>
        <button className="btn" onClick={onRefresh} disabled={loading} data-testid="restore-refresh">
          {loading ? "Refreshing…" : "Refresh"}
        </button>
      </div>
      <p className="hint">
        An Ignite backup preserves each library&apos;s current notebooks and pages (not its edit history). Restoring always creates new libraries; existing libraries are never
        replaced or merged.
      </p>
      {error && (
        <div role="alert" className={error.kind === "transient" ? "error-text" : "notice warn"} data-testid="restore-error" data-kind={error.kind}>
          {error.message}
          {error.kind === "transient" && jobs && <span className="hint block">Showing the last list that loaded.</span>}
        </div>
      )}
      {handlers && !accessRefused && <UploadPanel upload={actions.upload} picker={actions.picker} onSelect={handlers.onSelect} onUpload={handlers.onUpload} />}
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
            const status = stateLabel(job);
            const detailsId = `restore-details-${job.job_id}`;
            return (
              <li key={job.job_id} className="restore-job" data-testid="restore-job" data-state={job.state}>
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
                      {job.state === "ready" && <> · {job.usable ? `usable, expires in ${minutes(job.expires_in_s)}` : "no longer usable"}</>}
                    </div>
                  </div>
                  <div className="row-actions">
                    <button className="btn subtle" aria-expanded={open} aria-controls={detailsId} onClick={() => onToggle(job.job_id)} data-testid="restore-toggle">
                      {open ? "Hide details" : "Details"}
                    </button>
                    {handlers && job.state === "ready" && job.usable && cancel.confirming !== job.job_id && (
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
                <Outcome job={job} />
                {open && <Details job={job} id={detailsId} />}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

/** The Backup & Restore section (its own loading and refresh, independent of the rest). */
export function RestoreJobs({ api }: { api: ControlApi }) {
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
    actions.current?.reconcile(state.jobs, state.error?.kind ?? null);
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
  };
  return (
    <RestoreJobsView state={state} expanded={expanded} onToggle={toggle} onRefresh={() => void loader.current?.refresh()} actions={actionsState} handlers={handlers} />
  );
}
