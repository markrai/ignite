// Slice 1G-A: the read-only Backup & Restore section. The view is rendered to static markup
// (node environment, no DOM library); the refresh rules live in RestoreJobsLoader.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ControlError, type RestoreJob } from "../src/api/controlApi";
import { RestoreJobsView, formatBytes, type RestoreActionHandlers } from "../src/components/RestoreJobs";
import type { ActionsState, UploadState } from "../src/components/restoreActions";
import { LOOPBACK_EXPLANATION, RestoreJobsLoader, type RestoreJobsState } from "../src/components/restoreJobsLoader";

const SRC = "01a0de1f-0000-7000-8000-000000000001";
const DST = "01a0de1f-0000-7000-8000-000000000002";

function job(over: Partial<RestoreJob> = {}): RestoreJob {
  return {
    job_id: "01a0de1f-0000-7000-8000-00000000000a",
    state: "ready",
    usable: true,
    cleanup_pending: false,
    expires_in_s: 840,
    archive: { sha256: "ab".repeat(32), bytes: 5 * 1024 * 1024, format: "ignite-portable-backup", format_version: 1, snapshot_version: 1 },
    scope: "current_state_only",
    not_restored: ["operation history (each restored library starts at server sequence 0)", "device credentials and pairings"],
    totals: { libraries: 1, notebooks: 3, pages: 12, blobs: 2, blob_bytes: 9_000_000 },
    libraries: [
      {
        source_library_id: SRC,
        destination_library_id: DST,
        source_snapshot_sha256: "cd".repeat(32),
        source_snapshot_bytes: 4096,
        source_through_server_seq: 42,
        notebook_count: 3,
        page_count: 12,
        blob_count: 2,
      },
    ],
    warnings: [],
    result: null,
    ...over,
  };
}

const succeeded = (over: Partial<RestoreJob> = {}) =>
  job({
    state: "succeeded",
    usable: false,
    result: {
      outcome: "succeeded",
      publication: "complete",
      report: {
        archive_sha256: "ab".repeat(32),
        archive_bytes: 5 * 1024 * 1024,
        libraries: [{ source_library_id: SRC, library_id: DST, source_snapshot_sha256: "cd".repeat(32), restored_snapshot_sha256: "ef".repeat(32), notebook_count: 3, page_count: 12 }],
        blobs_installed: 1,
        blobs_already_present: 1,
      },
    },
    ...over,
  });

const failed = (publication: "none" | "unknown", over: Partial<RestoreJob> = {}) =>
  job({
    state: "failed",
    usable: false,
    result: {
      outcome: "failed",
      error: publication === "none" ? "conflict" : "store_commit_uncertain",
      message: publication === "none" ? "library already exists" : "The ledger refused or failed to publish the restore.",
      publication,
      unindexed_blob_files_possible: publication === "unknown",
    },
    ...over,
  });

function render(state: Partial<RestoreJobsState>, expanded: string[] = []): string {
  return renderToStaticMarkup(
    <RestoreJobsView state={{ jobs: null, loading: false, error: null, ...state }} expanded={new Set(expanded)} onToggle={() => undefined} onRefresh={() => undefined} />,
  );
}

/** Text content only (tags stripped, entities decoded). */
function text(html: string): string {
  return html
    .replace(/<[^>]+>/g, " ")
    .replace(/&#x27;|&#39;/g, "'")
    .replace(/&amp;/g, "&")
    .replace(/\s+/g, " ")
    .trim();
}

describe("1G-A Backup & Restore view", () => {
  it("loading, empty and the section heading", () => {
    const loading = render({ loading: true });
    expect(loading).toContain('data-testid="restore-loading"');
    expect(text(loading)).toContain("Backup & Restore");
    // Readable characters, asserted by code point (independent of how a terminal decodes them).
    expect(text(loading)).toContain("Loading restore jobs\u2026");
    expect(text(loading)).toContain("Refreshing\u2026");
    expect(loading).toMatch(/data-testid="restore-refresh"[^>]*disabled|disabled[^>]*data-testid="restore-refresh"/);
    const empty = text(render({ jobs: [] }));
    expect(empty).toContain("No restore jobs yet.");
    expect(empty).toContain("Upload an Ignite backup to verify it");
    expect(empty).not.toMatch(/upload now|backup was created/i);
  });

  it("a prepared job: verified, awaiting confirmation that the browser cannot give yet", () => {
    const html = render({ jobs: [job()] }, [job().job_id]);
    const t = text(html);
    expect(html).toContain('data-state="ready"');
    expect(t).toContain("Verified, awaiting confirmation");
    expect(t).toContain("5.0 MB backup \u00b7 1 library \u00b7 3 notebooks \u00b7 12 pages \u00b7 usable, expires in 14 minutes");
    expect(t).toContain("Confirming a restore is not yet available in the browser");
    // Source and proposed destination are distinct and labelled as proposed.
    expect(t).toContain(`Library in backup ${SRC}`);
    expect(t).toContain(`Proposed new library ${DST}`);
    expect(t).not.toContain("Restored as");
    expect(html.match(/<li>/g)?.length).toBeGreaterThanOrEqual(2);
    expect(t).toContain("Not included in a backup: operation history (each restored library starts at server sequence 0) device credentials and pairings");
    // Read-only: no confirm, cancel, upload or download control.
    expect(html.match(/<button/g)).toHaveLength(2); // Refresh + Details
    expect(t).not.toMatch(/\b(Confirm|Cancel|Upload|Download)\b/);
  });

  it("an executing job shows no invented progress", () => {
    const t = text(render({ jobs: [job({ state: "executing", usable: false })] }));
    expect(t).toContain("Restoration in progress");
    expect(t).toContain("refreshes automatically");
    expect(t).not.toMatch(/\d+\s*%/);
  });

  it("success: publication complete, restored identities, cleanup pending kept separate", () => {
    const html = render({ jobs: [succeeded({ cleanup_pending: true })] }, [job().job_id]);
    const t = text(html);
    expect(html).toContain('data-publication="complete"');
    expect(t).toContain("Restoration completed");
    expect(t).toContain("1 library restored as a new library. 1 background file added, 1 already present.");
    expect(t).toContain(`Restored as ${DST}`);
    expect(t).toContain("cleanup pending");
    expect(t).toContain("The restore is complete; the server is still removing its staged files.");
    expect(t).not.toMatch(/fail/i);
  });

  it("failure with publication none: nothing was published", () => {
    const html = render({ jobs: [failed("none")] }, [job().job_id]);
    const t = text(html);
    expect(html).toContain('data-publication="none"');
    expect(t).toContain("Restoration failed");
    expect(t).toContain("conflict : library already exists");
    expect(t).toContain("No libraries were published by this restore.");
    expect(t).toContain("Restoration failed");
    expect(t).not.toContain("uncertain");
    expect(t).toContain(`Proposed new library ${DST}`);
    expect(t).not.toContain("could not be confirmed");
  });

  it("failure with publication unknown is never presented as a definite failure to restore", () => {
    const html = render({ jobs: [failed("unknown")] }, [job().job_id]);
    const t = text(html);
    expect(html).toContain('data-publication="unknown"');
    expect(html).toContain('data-state="failed"'); // the server's state is unchanged
    expect(t).toContain("Restore outcome uncertain");
    expect(t).not.toContain("Restoration failed");
    expect(t).toContain("Publication outcome could not be confirmed. Check the destination libraries before attempting another restore");
    expect(t).not.toContain("No libraries were published");
    expect(t).toContain("Proposed new library (check whether it exists)");
    expect(t).toContain("Verified background files may remain on the server");
    expect(t).not.toMatch(/retry/i);
  });

  it("expired, withdrawn, and a finished job whose result is gone", () => {
    const t = text(
      render({
        jobs: [
          job({ job_id: "a", state: "expired", usable: false }),
          job({ job_id: "b", state: "cleanup_pending", usable: false, cleanup_pending: true }),
          job({ job_id: "c", state: "failed", usable: false, result: null }),
        ],
      }),
    );
    expect(t).toContain("expired before it was confirmed. Nothing was restored.");
    expect(t).toContain("Withdrawn, removing staged files");
    expect(t).toContain("cancelled or expired. Nothing was restored; the server is still removing its staged files.");
    expect(t).toContain("The result of this restore is no longer available.");
  });

  it("several libraries, long identifiers and messages render as escaped text", () => {
    const long = "x".repeat(300);
    const evil = '<img src=x onerror="alert(1)">';
    const many = job({
      totals: { libraries: 3, notebooks: 9, pages: 30, blobs: 0, blob_bytes: 0 },
      libraries: [1, 2, 3].map((n) => ({ ...job().libraries[0]!, source_library_id: `${SRC}-${n}${long}`, destination_library_id: `${DST}-${n}` })),
      warnings: [evil],
    });
    const html = render({ jobs: [many, failed("none", { job_id: "f", result: { outcome: "failed", error: "e", message: evil + long, publication: "none", unindexed_blob_files_possible: false } })] }, [many.job_id]);
    expect(html.match(/data-testid="restore-library"/g)).toHaveLength(3);
    expect(text(html)).toContain("3 libraries");
    expect(html).not.toContain("<img");
    expect(html).toContain("&lt;img");
    expect(html).toContain(long);
  });

  it("errors: loopback explanation, refused access and a transient failure over the last list", () => {
    const loopback = render({ error: { kind: "loopback", message: LOOPBACK_EXPLANATION } });
    expect(loopback).toContain('data-kind="loopback"');
    expect(text(loopback)).toContain("opened on the server itself");
    expect(loopback).not.toContain('data-testid="restore-job"');
    const transient = text(render({ jobs: [job()], error: { kind: "transient", message: "Ignite Server is not reachable." } }));
    expect(transient).toContain("Ignite Server is not reachable.");
    expect(transient).toContain("Showing the last list that loaded.");
    expect(transient).toContain("Verified, awaiting confirmation");
  });

  it("formats sizes", () => {
    expect([formatBytes(0), formatBytes(1023), formatBytes(1536), formatBytes(20 * 1024 * 1024), formatBytes(3 * 1024 ** 3)]).toEqual(["0 B", "1023 B", "1.5 KB", "20 MB", "3.0 GB"]);
  });
});

/** A load the test resolves or rejects by hand. */
function deferred() {
  let resolve!: (v: RestoreJob[]) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<RestoreJob[]>((ok, fail) => {
    resolve = ok;
    reject = fail;
  });
  return { promise, resolve, reject };
}

describe("1G-A restore jobs loader", () => {
  function loader() {
    const calls: ReturnType<typeof deferred>[] = [];
    const states: RestoreJobsState[] = [];
    const l = new RestoreJobsLoader(
      () => {
        const d = deferred();
        calls.push(d);
        return d.promise;
      },
      (s) => states.push(s),
    );
    return { l, calls, states };
  }

  it("never overlaps requests: a refresh while one is in flight joins it", async () => {
    const { l, calls } = loader();
    const a = l.refresh();
    const b = l.refresh();
    expect(calls).toHaveLength(1);
    expect(l.current.loading).toBe(true);
    calls[0]!.resolve([job()]);
    await Promise.all([a, b]);
    expect(l.current).toEqual({ jobs: [job()], loading: false, error: null });
    void l.refresh();
    expect(calls).toHaveLength(2);
  });

  it("an answer arriving after unmount is ignored", async () => {
    const { l, calls, states } = loader();
    const p = l.refresh();
    const seen = states.length;
    l.dispose();
    calls[0]!.resolve([job()]);
    await p;
    expect(states).toHaveLength(seen);
    await l.refresh();
    expect(calls).toHaveLength(1); // no request after dispose
  });

  it("transient failures keep the last list; refused access drops it", async () => {
    const { l, calls } = loader();
    let p = l.refresh();
    calls[0]!.resolve([job()]);
    await p;
    p = l.refresh();
    calls[1]!.reject(new ControlError(0, "unreachable", "Ignite Server is not reachable."));
    await p;
    expect(l.current.jobs).toEqual([job()]);
    expect(l.current.error?.kind).toBe("transient");
    p = l.refresh();
    calls[2]!.reject(new ControlError(401, "unauthorized", "not signed in"));
    await p;
    expect(l.current.jobs).toBeNull();
    expect(l.current.error?.kind).toBe("session");
    p = l.refresh();
    calls[3]!.reject(new ControlError(403, "loopback_required", "server message"));
    await p;
    expect(l.current.error).toEqual({ kind: "loopback", message: LOOPBACK_EXPLANATION });
    p = l.refresh();
    calls[4]!.reject(new ControlError(403, "forbidden", "administrator access required"));
    await p;
    expect(l.current.error).toEqual({ kind: "access", message: "administrator access required" });
  });

  it("polls only while a restore executes", async () => {
    const { l, calls } = loader();
    let p = l.refresh();
    calls[0]!.resolve([job(), succeeded({ job_id: "s" })]);
    await p;
    expect(l.executing).toBe(false);
    p = l.refresh();
    calls[1]!.resolve([job({ state: "executing" })]);
    await p;
    expect(l.executing).toBe(true);
  });
});

// ------------------------------------------------------------------ Slice 1G-B

const HANDLERS: RestoreActionHandlers = {
  onSelect: () => undefined,
  onUpload: () => undefined,
  onAskCancel: () => undefined,
  onKeep: () => undefined,
  onCancel: () => undefined,
};

function renderActions(state: Partial<RestoreJobsState>, actions: Partial<ActionsState> = {}, expanded: string[] = []): string {
  const full: ActionsState = { upload: { kind: "idle" }, cancel: { confirming: null, busy: null, notice: null }, picker: 0, ...actions };
  return renderToStaticMarkup(
    <RestoreJobsView
      state={{ jobs: null, loading: false, error: null, ...state }}
      expanded={new Set(expanded)}
      onToggle={() => undefined}
      onRefresh={() => undefined}
      actions={full}
      handlers={HANDLERS}
    />,
  );
}

const up = (upload: UploadState) => ({ upload });

describe("1G-B upload panel", () => {
  it("no file selected: a native file input, upload disabled", () => {
    const html = renderActions({ jobs: [] });
    expect(html).toContain('type="file"');
    expect(html).toContain('accept=".zip,application/zip"');
    expect(html).toMatch(/<button[^>]*disabled=""[^>]*data-testid="restore-upload-button"/);
    expect(text(html)).toContain("Upload and verify");
  });

  it("a selected file shows its name and size and enables upload", () => {
    const html = renderActions({ jobs: [] }, up({ kind: "selected", name: "ignite-backup.zip", size: 12_400_000, warning: null }));
    expect(text(html)).toContain("ignite-backup.zip \u00b7 12 MB");
    expect(html).not.toMatch(/disabled=""[^>]*data-testid="restore-upload-button"/);
  });

  it("empty or oversized files are refused before anything is sent", () => {
    const html = renderActions({ jobs: [] }, up({ kind: "invalid", name: "empty.zip", size: 0, message: "This file is empty. Choose an Ignite backup (.zip)." }));
    expect(html).toContain('data-testid="restore-file-invalid"');
    expect(html).toMatch(/<button[^>]*disabled=""[^>]*data-testid="restore-upload-button"/);
  });

  it("uploading: indeterminate, controls disabled, no percentage", () => {
    const html = renderActions({ jobs: [] }, up({ kind: "uploading", name: "b.zip", size: 1024 }));
    const t = text(html);
    expect(t).toContain("Uploading and verifying\u2026");
    expect(t).toContain("keep this page open");
    expect(t).not.toMatch(/\d+\s*%/);
    expect(html).toMatch(/<input[^>]*type="file"[^>]*disabled=""|<input[^>]*disabled=""[^>]*type="file"/);
    expect(html).toMatch(/<button[^>]*disabled=""[^>]*data-testid="restore-upload-button"/);
    expect(html).toContain('aria-busy="true"');
  });

  it("a verified preview: real counts, and an explicit statement that nothing was restored", () => {
    const many = job({
      totals: { libraries: 2, notebooks: 18, pages: 94, blobs: 3, blob_bytes: 3 * 1024 * 1024 },
      libraries: [1, 2].map((n) => ({ ...job().libraries[0]!, source_library_id: `${SRC}-${n}`, destination_library_id: `${DST}-${n}` })),
      warnings: ["restored libraries receive new library identities at import"],
    });
    const html = renderActions({ jobs: [many] }, up({ kind: "prepared", name: "b.zip", job: many }), [many.job_id]);
    const t = text(html);
    expect(t).toContain("Verification complete");
    expect(t).toContain("2 libraries \u00b7 18 notebooks \u00b7 94 pages \u00b7 3 background files (3.0 MB)");
    expect(t).toContain("This is a preview. No notebooks have been restored.");
    expect(t).toContain("Confirming a restore will be available in a later version.");
    expect(t).toContain("If this page is reloaded it can still be reviewed or cancelled");
    // Shared details: both mappings, warnings and exclusions.
    expect(html.match(/data-testid="restore-library"/g)).toHaveLength(2);
    expect(t).toContain(`Library in backup ${SRC}-1 Proposed new library ${DST}-1`);
    expect(t).toContain("Notes from verification: restored libraries receive new library identities at import");
    expect(t).toContain("Not included in a backup:");
    // Nothing can confirm.
    expect(t).not.toMatch(/\bConfirm\b|Restore now/);
  });

  it("a refused archive keeps the server's code; an interrupted upload is uncertain", () => {
    const rejected = text(
      renderActions(
        { jobs: [] },
        up({ kind: "failed", name: "bad.zip", size: 9, error: { kind: "rejected", title: "Backup could not be verified", message: "not a ZIP archive Nothing was prepared.", code: "archive_invalid: zip_not_zip" } }),
      ),
    );
    expect(rejected).toContain("Backup could not be verified");
    expect(rejected).toContain("archive_invalid: zip_not_zip");
    const html = renderActions(
      { jobs: [] },
      up({ kind: "failed", name: "b.zip", size: 9, error: { kind: "uncertain", title: "Upload outcome unknown", message: "Refresh the list before uploading again.", code: null } }),
    );
    expect(html).toContain('data-kind="uncertain"');
    expect(text(html)).toContain("Refresh the list before uploading again.");
  });

  it("no upload control when restore access is refused", () => {
    for (const kind of ["session", "loopback", "access"] as const) {
      expect(renderActions({ error: { kind, message: "refused" } }), kind).not.toContain('data-testid="restore-upload"');
    }
    expect(renderActions({ jobs: [job()], error: { kind: "transient", message: "down" } })).toContain('data-testid="restore-upload"');
  });
});

describe("1G-B cancel controls", () => {
  it("only a usable prepared job offers Cancel", () => {
    const html = renderActions({
      jobs: [
        job({ job_id: "r" }),
        job({ job_id: "x", state: "ready", usable: false }),
        job({ job_id: "e", state: "executing", usable: false }),
        succeeded({ job_id: "s" }),
        failed("none", { job_id: "f" }),
        job({ job_id: "c", state: "cleanup_pending", usable: false }),
      ],
    });
    expect(html.match(/data-testid="restore-cancel"/g)).toHaveLength(1);
  });

  it("asks before discarding, disables while busy, and reports the outcome", () => {
    const asking = renderActions({ jobs: [job()] }, { cancel: { confirming: job().job_id, busy: null, notice: null } });
    expect(text(asking)).toContain("Discard this prepared restore? Nothing has been restored and no library will change.");
    expect(asking).toContain('data-testid="restore-cancel-yes"');
    expect(asking).not.toContain('data-testid="restore-cancel"'); // the question replaces the button
    const busy = renderActions({ jobs: [job()] }, { cancel: { confirming: null, busy: job().job_id, notice: null } });
    expect(busy).toMatch(/<button[^>]*disabled=""[^>]*data-testid="restore-cancel"/);
    expect(text(busy)).toContain("Cancelling\u2026");
    const done = renderActions({ jobs: [] }, { cancel: { confirming: null, busy: null, notice: { jobId: "j", tone: "bad", text: "This restore has started and can no longer be cancelled." } } });
    expect(done).toContain('data-tone="bad"');
    expect(text(done)).toContain("can no longer be cancelled");
  });
});

describe("1G-B confirmation secret", () => {
  it("the view has no input for it, so no rendering can contain it", () => {
    const SECRET = "S3cr3tS3cr3tS3cr3tS3cr3tS3cr3tS3cr3tS3cr3t1";
    const prepared = job();
    // Even a summary object carrying an extra field would not be rendered field by field.
    const leaky = { ...prepared, confirmation: SECRET } as RestoreJob;
    const html = renderActions({ jobs: [leaky] }, up({ kind: "prepared", name: "b.zip", job: leaky }), [prepared.job_id]);
    expect(html).not.toContain(SECRET);
  });
});

describe("1G-B loader: a fresh upload is never overwritten by an older list", () => {
  it("upsert shows the job at once; a list requested before it is discarded; the next list applies", async () => {
    const calls: ReturnType<typeof deferred>[] = [];
    const l = new RestoreJobsLoader(
      () => {
        const d = deferred();
        calls.push(d);
        return d.promise;
      },
      () => undefined,
    );
    const old = l.refresh(); // in flight before the upload finished
    l.upsert(job({ job_id: "new" }));
    expect(l.current.jobs?.map((j) => j.job_id)).toEqual(["new"]);
    const after = l.refreshAfter();
    calls[0]!.resolve([]); // the old list (without the new job)
    await old;
    expect(l.current.jobs?.map((j) => j.job_id)).toEqual(["new"]); // not overwritten
    await Promise.resolve();
    await Promise.resolve();
    expect(calls).toHaveLength(2); // a fresh request followed
    calls[1]!.resolve([job({ job_id: "new" }), job({ job_id: "other" })]);
    await after;
    expect(l.current.jobs?.map((j) => j.job_id)).toEqual(["new", "other"]);
  });
});
