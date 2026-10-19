// Server Settings → Backup & Restore → Backup (Slice 1G-D2). The server prepares and verifies a
// portable backup; the administrator downloads it with an ordinary same-origin link (the
// browser streams it to disk; this page never reads the archive) and may discard the server's
// temporary copy. Every server string is rendered as text.

import { useEffect, useRef, useState } from "react";
import type { BackupExport, BackupExportState, ControlApi } from "../api/controlApi";
import { BackupExportsController, INITIAL_EXPORTS_STATE, canCreate, downloadHref, type BackupExportsState, type CreateState } from "./backupExportsController";
import { formatBytes } from "./RestoreJobs";

const STATE_LABEL: Record<BackupExportState, string> = {
  preparing: "Preparing backup",
  ready: "Backup ready",
  failed: "Backup failed",
  expired: "Backup expired",
  cleanup_pending: "Cleanup pending",
};

/** Mirrors `backup_archive::NOT_RESTORED` (what a portable backup does not contain). */
export const NOT_IN_BACKUP: readonly string[] = [
  "operation history (each restored library starts at server sequence 0)",
  "deleted-object history (tombstones of deleted notebooks, pages, strokes, text and links)",
  "device credentials and pairings",
  "server identity",
  "browser replica state",
  "edits not yet synchronized to this server",
];

/** Approximate time left (server-reported, counted down locally; the server decides). */
function remaining(deadline: number | undefined, now: number): string {
  if (deadline === undefined) return "unknown";
  const s = Math.max(0, (deadline - now) / 1000);
  if (s < 60) return "less than a minute";
  const m = Math.ceil(s / 60);
  return m === 1 ? "about 1 minute" : `about ${m} minutes`;
}

export interface BackupExportHandlers {
  onCreate: () => void;
  onRefresh: () => void;
  onDownload: (exportId: string) => void;
  onAskDismiss: (exportId: string) => void;
  onKeep: () => void;
  onDismiss: (exportId: string) => void;
}

function CreateNotice({ create }: { create: CreateState }) {
  switch (create.kind) {
    case "sending":
      return (
        <p className="hint" data-testid="backup-create-notice" data-kind="sending">
          Requesting a backup…
        </p>
      );
    case "accepted":
      return null; // the preparing export is shown below
    case "uncertain":
      return (
        <div className="notice warn" data-testid="backup-create-notice" data-kind="uncertain">
          <strong>Backup request outcome unknown.</strong>
          <div>
            The connection ended before the server answered, so the server may already be preparing a backup. It is not requested again automatically.{" "}
            {create.checkFailed ? "The list of backups could not be checked: choose Refresh to check again." : "Checking the server's list of backups…"}
          </div>
        </div>
      );
    case "uncertain_found":
      return (
        <div className="notice warn" data-testid="backup-create-notice" data-kind="uncertain_found">
          <strong>Backup request outcome unknown.</strong>
          <div>The server reports the backup shown below. It may be the one you requested.</div>
        </div>
      );
    case "uncertain_none":
      return (
        <div className="notice warn" data-testid="backup-create-notice" data-kind="uncertain_none">
          <strong>Backup request outcome unknown.</strong>
          <div>The server currently reports no backup from that request. You can create a backup again.</div>
        </div>
      );
    case "refused":
      return (
        <div className={`notice ${create.error.kind === "capacity" ? "warn" : "bad"}`} data-testid="backup-create-notice" data-kind={`refused-${create.error.kind}`}>
          <strong>{create.error.title}</strong>
          <div>{create.error.message}</div>
          {create.error.code && <span className="hint block mono">{create.error.code}</span>}
        </div>
      );
    default:
      return null;
  }
}

function ExportSummary({ e }: { e: BackupExport }) {
  if (!e.archive || !e.totals) return null;
  return (
    <dl className="kv backup-facts" data-testid="backup-facts">
      <dt>Libraries</dt>
      <dd data-testid="backup-libraries">{e.totals.libraries}</dd>
      <dt>Notebooks</dt>
      <dd data-testid="backup-notebooks">{e.totals.notebooks}</dd>
      <dt>Pages</dt>
      <dd data-testid="backup-pages">{e.totals.pages}</dd>
      <dt>Background files</dt>
      <dd data-testid="backup-blobs">
        {e.totals.blobs} ({formatBytes(e.totals.blob_bytes)})
      </dd>
      <dt>Archive</dt>
      <dd>
        <span className="mono">{e.archive.file_name}</span> · <span data-testid="backup-size">{formatBytes(e.archive.bytes)}</span>{" "}
        <span className="hint">({e.archive.bytes.toLocaleString("en-US")} bytes)</span>
      </dd>
      <dt>SHA-256</dt>
      <dd className="mono backup-sha" data-testid="backup-sha">
        {e.archive.sha256}
      </dd>
      <dt>Format</dt>
      <dd>
        {e.archive.format} v{e.archive.format_version}
      </dd>
    </dl>
  );
}

function ExportRow({
  e,
  deadline,
  now,
  state,
  handlers,
}: {
  e: BackupExport;
  deadline: number | undefined;
  now: number;
  state: BackupExportsState;
  handlers: BackupExportHandlers;
}) {
  const { dismiss } = state;
  const href = downloadHref(e, deadline, now);
  const lapsed = e.state === "ready" && !href;
  const label = lapsed ? "Checking whether the backup expired" : STATE_LABEL[e.state];
  const tone = lapsed ? "executing" : e.state === "preparing" ? "executing" : e.state === "ready" ? "succeeded" : e.state === "failed" ? "failed" : "expired";
  const discardable = e.state === "ready" || e.state === "failed" || e.state === "expired";
  const busy = dismiss.busy === e.export_id;
  return (
    <li className="restore-job backup-export" data-testid="backup-export" data-state={e.state} data-export-id={e.export_id}>
      <div className="row">
        <div>
          <div className="row-title">
            <span className={`status status-${tone}`} data-testid="backup-state">
              {label}
            </span>
          </div>
          {e.state === "ready" && href && (
            <div className="hint" data-testid="backup-expiry">
              Available on the server for {remaining(deadline, now)}.
            </div>
          )}
        </div>
        <div className="row-actions">
          {href ? (
            <a
              className="btn primary"
              href={href}
              download="ignite-backup.zip"
              referrerPolicy="no-referrer"
              onClick={() => handlers.onDownload(e.export_id)}
              data-testid="backup-download"
            >
              Download backup
            </a>
          ) : (
            e.state === "ready" && (
              <button className="btn primary" disabled data-testid="backup-download-disabled">
                Download backup
              </button>
            )
          )}
          {discardable && dismiss.confirming !== e.export_id && (
            <button className="btn" onClick={() => handlers.onAskDismiss(e.export_id)} disabled={dismiss.busy !== null} data-testid="backup-discard">
              {busy ? "Discarding…" : "Discard prepared backup…"}
            </button>
          )}
        </div>
      </div>
      {e.state === "preparing" && (
        <p className="hint" data-testid="backup-preparing">
          Preparing and verifying your backup… The server checks the finished archive before offering it. This page follows its progress automatically.
        </p>
      )}
      {e.state === "ready" && (
        <>
          <ExportSummary e={e} />
          <p className="hint" data-testid="backup-consistency">
            Each library is captured at its own snapshot point. This is not one atomic snapshot across all libraries. The backup contains each library&apos;s current notebooks
            and pages as stored on this server, not a full system backup.
          </p>
          {state.downloadStarted === e.export_id && (
            <div role="status" className="notice ok" data-testid="backup-download-started">
              Download started in your browser. Check your browser&apos;s downloads to make sure <span className="mono">{e.archive?.file_name}</span> was saved completely;
              you can compare its SHA-256 with the value above. The backup stays available here until it expires or you discard it.
            </div>
          )}
        </>
      )}
      {lapsed && (
        <p className="hint" data-testid="backup-lapsed">
          The time the server reported for this backup has run out. Checking with the server…
        </p>
      )}
      {e.state === "failed" && e.error && (
        <div className="notice bad" data-testid="backup-failed">
          <div>The server could not prepare this backup. Nothing was offered for download.</div>
          <div>
            <span className="mono">{e.error.code}</span>: {e.error.message}
          </div>
        </div>
      )}
      {e.state === "expired" && (
        <p className="hint" data-testid="backup-expired">
          This backup expired and can no longer be downloaded. Create a new backup if you still need one. A ZIP you already saved is not affected.
        </p>
      )}
      {e.state === "cleanup_pending" && (
        <p className="hint" data-testid="backup-cleanup">
          This backup was withdrawn and can no longer be downloaded. The server is still removing its temporary file.
        </p>
      )}
      {dismiss.confirming === e.export_id && (
        <div className="notice warn" role="group" aria-label="Discard this prepared backup?" data-testid="backup-discard-confirm">
          <div>This removes the temporary backup from the server. It does not delete your notebooks or a ZIP already saved on your computer.</div>
          <div className="row-actions">
            <button className="btn primary" onClick={() => handlers.onDismiss(e.export_id)} disabled={dismiss.busy !== null} data-testid="backup-discard-yes">
              Discard prepared backup
            </button>
            <button className="btn" onClick={handlers.onKeep} disabled={dismiss.busy !== null} data-testid="backup-discard-no">
              Keep
            </button>
          </div>
        </div>
      )}
    </li>
  );
}

export function BackupExportsView({ state, now, handlers }: { state: BackupExportsState; now: number; handlers: BackupExportHandlers }) {
  const { exports, loading, error, create, dismiss } = state;
  const accessRefused = error !== null && error.kind !== "transient";
  const sending = create.kind === "sending";
  const holds = exports?.some((e) => e.state === "preparing" || e.state === "ready" || e.state === "expired" || e.state === "cleanup_pending") ?? false;
  return (
    <div className="backup-exports" data-testid="backup-section" role="region" aria-labelledby="backup-heading">
      <div className="card-head">
        <h3 id="backup-heading">Backup</h3>
        <button className="btn" onClick={handlers.onRefresh} disabled={loading} aria-label="Refresh backups" data-testid="backup-refresh">
          {loading ? "Refreshing…" : "Refresh"}
        </button>
      </div>
      <p className="hint">Create a portable backup of the libraries currently stored on this Ignite Server.</p>
      {error && (
        <div role="alert" className={error.kind === "transient" ? "error-text" : "notice warn"} data-testid="backup-error" data-kind={error.kind}>
          {error.message}
          {error.kind === "transient" && exports && <span className="hint block">Showing the last list that loaded.</span>}
        </div>
      )}
      {!accessRefused && (
        <div className="row-actions backup-create">
          <button className="btn primary" onClick={handlers.onCreate} disabled={!canCreate(state)} aria-busy={sending} data-testid="backup-create">
            {sending ? "Requesting backup…" : "Create backup"}
          </button>
        </div>
      )}
      {!accessRefused && holds && create.kind !== "sending" && (
        <p className="hint" data-testid="backup-one-at-a-time">
          One backup is kept per administrator. Download or discard the backup below (or wait for it to expire) before creating another.
        </p>
      )}
      <div role="status" aria-live="polite">
        <CreateNotice create={create} />
        {dismiss.notice && (
          <div className={`notice ${dismiss.notice.tone}`} data-testid="backup-discard-notice" data-tone={dismiss.notice.tone}>
            {dismiss.notice.text}
          </div>
        )}
      </div>
      {!exports && !error && (
        <p className="hint" data-testid="backup-loading">
          Loading backups…
        </p>
      )}
      {exports && exports.length === 0 && (
        <p className="hint" data-testid="backup-empty">
          No backup is prepared on the server. Prepared backups are temporary: the server removes them after a while and when it restarts.
        </p>
      )}
      {exports && exports.length > 0 && (
        <ul className="rows" aria-live="polite">
          {exports.map((e) => (
            <ExportRow key={e.export_id} e={e} deadline={state.deadlines[e.export_id]} now={now} state={state} handlers={handlers} />
          ))}
        </ul>
      )}
      {exports && exports.some((e) => e.state === "ready") && (
        <div className="hint" data-testid="backup-not-included">
          Not included in a backup:
          <ul className="restore-not-restored">
            {NOT_IN_BACKUP.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        </div>
      )}
      <p className="hint">To restore a saved backup later, select its ZIP in Restore below.</p>
    </div>
  );
}

/** The Backup half of Backup & Restore: its own controller, independent of the restore one. */
export function BackupExports({ api }: { api: ControlApi }) {
  const [state, setState] = useState<BackupExportsState>(INITIAL_EXPORTS_STATE);
  const [now, setNow] = useState(() => Date.now());
  const controller = useRef<BackupExportsController | null>(null);

  useEffect(() => {
    const c = new BackupExportsController(api, (s) => {
      setState(s);
      setNow(Date.now());
    });
    controller.current = c;
    const first = setTimeout(() => void c.refresh(), 0);
    return () => {
      clearTimeout(first);
      c.dispose();
      if (controller.current === c) controller.current = null;
    };
  }, [api]);

  // The remaining time is counted down locally while a backup is ready (a display only).
  const ready = state.exports?.some((e) => e.state === "ready") ?? false;
  useEffect(() => {
    if (!ready) return;
    const t = setInterval(() => setNow(Date.now()), 15_000);
    return () => clearInterval(t);
  }, [ready]);

  const handlers: BackupExportHandlers = {
    onCreate: () => void controller.current?.create(),
    onRefresh: () => void controller.current?.refresh(),
    onDownload: (id) => controller.current?.downloadStarted(id),
    onAskDismiss: (id) => controller.current?.askDismiss(id),
    onKeep: () => controller.current?.keep(),
    onDismiss: (id) => void controller.current?.dismiss(id),
  };
  return <BackupExportsView state={state} now={now} handlers={handlers} />;
}
