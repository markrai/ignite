// Slice 1G-A: Server Settings → Backup & Restore. Read-only: lists this administrator's restore
// jobs (GET only). Uploading, confirming and cancelling come in later slices, so no control for
// them is shown. Every server string is rendered as text.

import { useEffect, useRef, useState } from "react";
import type { ControlApi, RestoreJob, RestoreJobState } from "../api/controlApi";
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

export function RestoreJobsView({
  state,
  expanded,
  onToggle,
  onRefresh,
}: {
  state: RestoreJobsState;
  expanded: ReadonlySet<string>;
  onToggle: (jobId: string) => void;
  onRefresh: () => void;
}) {
  const { jobs, loading, error } = state;
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
      {!jobs && !error && (
        <p className="hint" data-testid="restore-loading">
          Loading restore jobs…
        </p>
      )}
      {jobs && jobs.length === 0 && (
        <div data-testid="restore-empty">
          <p>No restore jobs yet.</p>
          <p className="hint">Uploading a backup from the browser will be added in a later version.</p>
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
                  </div>
                </div>
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
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const loader = useRef<RestoreJobsLoader | null>(null);

  useEffect(() => {
    const l = new RestoreJobsLoader(() => api.restoreJobs(), setState);
    loader.current = l;
    const first = setTimeout(() => void l.refresh(), 0);
    return () => {
      clearTimeout(first);
      l.dispose();
      if (loader.current === l) loader.current = null;
    };
  }, [api]);

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

  return <RestoreJobsView state={state} expanded={expanded} onToggle={toggle} onRefresh={() => void loader.current?.refresh()} />;
}
