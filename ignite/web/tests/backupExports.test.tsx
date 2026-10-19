// Slice 1G-D2: Backup (server-prepared export, native download). The controller's rules are
// tested with deferred promises and fake timers; the view is rendered to static markup (node
// environment, no DOM library), like the restore section.

import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ControlError, exportDownloadPath, type BackupExport, type BackupExportDismissed, type ControlApi } from "../src/api/controlApi";
import { BackupExportsView, type BackupExportHandlers } from "../src/components/BackupExports";
import {
  BACKUP_LOOPBACK_EXPLANATION,
  BackupExportsController,
  INITIAL_EXPORTS_STATE,
  PREPARING_POLL_MS,
  canCreate,
  downloadHref,
  type BackupExportsState,
} from "../src/components/backupExportsController";
import { RestoreJobsView } from "../src/components/RestoreJobs";

const A = "01a0de1f-0000-7000-8000-0000000000a1";
const B = "01a0de1f-0000-7000-8000-0000000000b2";

function preparing(id = A): BackupExport {
  return { export_id: id, state: "preparing", expires_in_s: null, consistency: "per_library_snapshot", archive: null, totals: null, error: null, download: null };
}

function ready(id = A, over: Partial<BackupExport> = {}): BackupExport {
  return {
    export_id: id,
    state: "ready",
    expires_in_s: 900,
    consistency: "per_library_snapshot",
    archive: { file_name: "ignite-backup.zip", bytes: 1_234_567, sha256: "ab".repeat(32), format: "ignite-portable-backup", format_version: 1 },
    totals: { libraries: 2, notebooks: 18, pages: 94, blobs: 3, blob_bytes: 123_456 },
    error: null,
    download: exportDownloadPath(id),
    ...over,
  };
}

function failed(id = A): BackupExport {
  return { ...preparing(id), state: "failed", expires_in_s: 900, error: { code: "io_error", message: "A filesystem error stopped the export." } };
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((ok, fail) => {
    resolve = ok;
    reject = fail;
  });
  return { promise, resolve, reject };
}

/** A controller over a scripted API: every call returns a deferred the test settles. */
function harness(now = () => 1_000_000) {
  const lists: ReturnType<typeof deferred<BackupExport[]>>[] = [];
  const starts: ReturnType<typeof deferred<BackupExport>>[] = [];
  const dismissals: { id: string; d: ReturnType<typeof deferred<BackupExportDismissed>> }[] = [];
  const api: Pick<ControlApi, "listBackupExports" | "startBackupExport" | "dismissBackupExport"> = {
    listBackupExports: () => {
      const d = deferred<BackupExport[]>();
      lists.push(d);
      return d.promise;
    },
    startBackupExport: () => {
      const d = deferred<BackupExport>();
      starts.push(d);
      return d.promise;
    },
    dismissBackupExport: (id) => {
      const d = deferred<BackupExportDismissed>();
      dismissals.push({ id, d });
      return d.promise;
    },
  };
  const states: BackupExportsState[] = [];
  const c = new BackupExportsController(api, (s) => states.push(s), now);
  return { c, lists, starts, dismissals, states };
}

/** Let settled promises run (no timers involved). */
const flush = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};

async function loaded(h: ReturnType<typeof harness>, list: BackupExport[]) {
  const p = h.c.refresh();
  h.lists.at(-1)!.resolve(list);
  await p;
  await flush();
}

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("1G-D2 backup controller: creating", () => {
  it("a double click sends one POST; a 202 shows the preparing export, never a ready one", async () => {
    const h = harness();
    await loaded(h, []);
    const a = h.c.create();
    const b = h.c.create();
    expect(h.starts).toHaveLength(1);
    expect(h.c.current.create.kind).toBe("sending");
    expect(canCreate(h.c.current)).toBe(false);
    h.starts[0]!.resolve(preparing());
    await Promise.all([a, b]);
    await flush();
    expect(h.c.current.create).toEqual({ kind: "accepted", exportId: A });
    expect(h.c.current.exports).toEqual([preparing()]);
    expect(h.c.current.exports!.some((e) => e.state === "ready")).toBe(false);
    expect(canCreate(h.c.current)).toBe(false); // one export per administrator
    await h.c.create();
    expect(h.starts).toHaveLength(1);
  });

  it("an unanswered POST is never retried; a fresh list settles it as found", async () => {
    const h = harness();
    await loaded(h, [failed(B)]);
    const p = h.c.create();
    h.starts[0]!.reject(new ControlError(0, "interrupted", "The connection ended before the server answered."));
    await p;
    await flush();
    expect(h.c.current.create).toEqual({ kind: "uncertain", checkFailed: false });
    expect(canCreate(h.c.current)).toBe(false);
    expect(h.lists).toHaveLength(2); // the check
    h.lists[1]!.resolve([preparing(A), failed(B)]);
    await flush();
    expect(h.c.current.create.kind).toBe("uncertain_found");
    expect(h.c.current.exports!.map((e) => e.export_id)).toEqual([B, A]);
    await vi.advanceTimersByTimeAsync(10 * PREPARING_POLL_MS);
    expect(h.starts).toHaveLength(1); // never resent
  });

  it("an unanswered POST whose list shows nothing new allows a new explicit request only", async () => {
    const h = harness();
    await loaded(h, [failed(B)]);
    const p = h.c.create();
    h.starts[0]!.reject(new TypeError("network"));
    await p;
    h.lists[1]!.resolve([failed(B)]);
    await flush();
    expect(h.c.current.create.kind).toBe("uncertain_none");
    expect(canCreate(h.c.current)).toBe(true);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(h.starts).toHaveLength(1);
  });

  it("a malformed 202 is treated as an unknown outcome, not as a refusal", async () => {
    const h = harness();
    await loaded(h, []);
    const p = h.c.create();
    h.starts[0]!.reject(new ControlError(0, "unsupported_response", "x"));
    await p;
    expect(h.c.current.create.kind).toBe("uncertain");
  });

  it("if the check itself fails, the outcome stays unknown until a manual refresh answers", async () => {
    const h = harness();
    await loaded(h, []);
    const p = h.c.create();
    h.starts[0]!.reject(new ControlError(0, "interrupted", "x"));
    await p;
    h.lists[1]!.reject(new ControlError(0, "unreachable", "Ignite Server is not reachable."));
    await flush();
    expect(h.c.current.create).toEqual({ kind: "uncertain", checkFailed: true });
    expect(h.c.current.error?.kind).toBe("transient");
    expect(canCreate(h.c.current)).toBe(false);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(h.lists).toHaveLength(2); // no automatic loop
    const r = h.c.refresh();
    h.lists[2]!.resolve([ready(A)]);
    await r;
    expect(h.c.current.create.kind).toBe("uncertain_found");
  });

  it("an older list answered after the 202 never hides the new export", async () => {
    const h = harness();
    await loaded(h, []);
    const old = h.c.refresh(); // in flight before the click
    const p = h.c.create();
    h.starts[0]!.resolve(preparing());
    await p;
    h.lists[1]!.resolve([]); // answered for a state before the export existed
    await old;
    await flush();
    expect(h.c.current.exports).toEqual([preparing()]);
  });

  it("a stale preparing answer never replaces a newer ready one", async () => {
    const h = harness();
    await loaded(h, [ready(A)]);
    await loaded(h, [preparing(A)]);
    expect(h.c.current.exports![0]!.state).toBe("ready");
    // Nor does a 202 arriving after a list already showed it ready.
    const h2 = harness();
    await loaded(h2, []);
    const p = h2.c.create();
    const r = h2.c.refresh();
    h2.lists[1]!.resolve([ready(A)]);
    await r;
    h2.starts[0]!.resolve(preparing(A));
    await p;
    await flush();
    expect(h2.c.current.exports![0]!.state).toBe("ready");
  });

  it("429 variants are explained, keep the list, and refresh it to show what the server holds", async () => {
    const cases: [string, RegExp][] = [
      ["export_exists", /already have an export\. Download or dismiss it/],
      ["export_busy", /Another backup is currently being prepared/],
      ["too_many_exports", /reached its export capacity/],
    ];
    for (const [code, text] of cases) {
      const h = harness();
      await loaded(h, []);
      const p = h.c.create();
      h.starts[0]!.reject(new ControlError(429, code, "server text"));
      await p;
      const c = h.c.current.create;
      expect(c.kind).toBe("refused");
      if (c.kind !== "refused") continue;
      expect(c.error.kind).toBe("capacity");
      expect(c.error.message).toMatch(text);
      expect(c.error.code).toBe(code);
      expect(h.lists).toHaveLength(2);
    }
  });

  it("other refusals: storage, shutdown, server error, CSRF; access refusals drop the list", async () => {
    const outcome = async (status: number, code: string) => {
      const h = harness();
      await loaded(h, [failed(B)]);
      const p = h.c.create();
      h.starts[0]!.reject(new ControlError(status, code, "server says"));
      await p;
      return h.c.current;
    };
    for (const [status, code, kind] of [
      [507, "insufficient_storage", "storage"],
      [503, "shutting_down", "server"],
      [500, "internal", "server"],
      [403, "csrf", "csrf"],
    ] as const) {
      const s = await outcome(status, code);
      expect(s.create.kind === "refused" && s.create.error.kind, code).toBe(kind);
      expect(s.exports, code).toEqual([failed(B)]); // not an access problem
    }
    for (const [status, code, kind] of [
      [401, "unauthorized", "session"],
      [403, "loopback_required", "loopback"],
      [403, "browser_session_required", "access"],
      [403, "forbidden", "access"],
    ] as const) {
      const s = await outcome(status, code);
      expect(s.exports, code).toBeNull();
      expect(s.error?.kind, code).toBe(kind);
    }
    expect((await outcome(403, "loopback_required")).error?.message).toBe(BACKUP_LOOPBACK_EXPLANATION);
  });
});

describe("1G-D2 backup controller: freshness and polling", () => {
  it("existing exports are recovered from the list (after a reload) and followed while preparing", async () => {
    const h = harness();
    await loaded(h, [preparing()]);
    expect(canCreate(h.c.current)).toBe(false);
    expect(h.lists).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(PREPARING_POLL_MS - 1);
    expect(h.lists).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(h.lists).toHaveLength(2);
    // No overlap: nothing new is requested while that one is unanswered.
    await vi.advanceTimersByTimeAsync(10 * PREPARING_POLL_MS);
    expect(h.lists).toHaveLength(2);
    h.lists[1]!.resolve([ready()]);
    await flush();
    await vi.advanceTimersByTimeAsync(10 * PREPARING_POLL_MS);
    expect(h.lists).toHaveLength(2); // terminal: polling stopped
  });

  it("polls only while preparing; failed, expired and cleanup-pending exports are not polled", async () => {
    const h = harness();
    await loaded(h, [failed(), { ...ready(B), state: "expired", download: null, expires_in_s: 0 }]);
    await vi.advanceTimersByTimeAsync(10 * PREPARING_POLL_MS);
    expect(h.lists).toHaveLength(1);
  });

  it("polling stops on unmount and nothing is applied afterwards", async () => {
    const h = harness();
    await loaded(h, [preparing()]);
    await vi.advanceTimersByTimeAsync(PREPARING_POLL_MS);
    expect(h.lists).toHaveLength(2);
    const seen = h.states.length;
    h.c.dispose();
    h.lists[1]!.resolve([ready()]);
    await flush();
    await vi.advanceTimersByTimeAsync(10 * PREPARING_POLL_MS);
    expect(h.lists).toHaveLength(2);
    expect(h.states).toHaveLength(seen);
  });

  it("a failed refresh keeps the last list; an access refusal clears it and stops polling", async () => {
    const h = harness();
    await loaded(h, [ready(), preparing(B)]);
    await vi.advanceTimersByTimeAsync(PREPARING_POLL_MS);
    h.lists[1]!.reject(new ControlError(0, "unreachable", "Ignite Server is not reachable."));
    await flush();
    expect(h.c.current.exports).toHaveLength(2);
    expect(h.c.current.error?.kind).toBe("transient");
    await vi.advanceTimersByTimeAsync(PREPARING_POLL_MS); // still preparing as far as we know
    expect(h.lists).toHaveLength(3);
    h.lists[2]!.reject(new ControlError(401, "unauthorized", "not signed in"));
    await flush();
    expect(h.c.current.exports).toBeNull();
    expect(h.c.current.deadlines).toEqual({});
    expect(h.c.current.error?.kind).toBe("session");
    await vi.advanceTimersByTimeAsync(10 * PREPARING_POLL_MS);
    expect(h.lists).toHaveLength(3);
  });

  it("an earlier successful answer cannot bring back a list dropped for lack of access", async () => {
    const h = harness();
    await loaded(h, [ready()]);
    const inFlight = h.c.refresh();
    h.c.askDismiss(A);
    const d = h.c.dismiss(A);
    h.dismissals[0]!.d.reject(new ControlError(403, "forbidden", "administrator access required"));
    await d;
    expect(h.c.current.exports).toBeNull();
    h.lists[1]!.resolve([ready()]);
    await inFlight;
    await flush();
    expect(h.c.current.exports).toBeNull();
  });

  it("a ready export's reported time is counted locally; when it runs out the server is asked", async () => {
    let t = 1_000_000;
    const h = harness(() => t);
    await loaded(h, [ready(A, { expires_in_s: 60 })]);
    const deadline = h.c.current.deadlines[A]!;
    expect(deadline).toBe(1_000_000 + 60_000);
    const e = h.c.current.exports![0]!;
    expect(downloadHref(e, deadline, t)).toBe(exportDownloadPath(A));
    expect(downloadHref(e, deadline, deadline)).toBeNull(); // disabled locally at the deadline
    t = deadline + 1000;
    await vi.advanceTimersByTimeAsync(61_000);
    expect(h.lists).toHaveLength(2); // the server decides
    h.lists[1]!.resolve([{ ...ready(A), state: "expired", expires_in_s: 0, download: null }]);
    await flush();
    expect(h.c.current.exports![0]!.state).toBe("expired");
    expect(h.c.current.deadlines).toEqual({});
  });
});

describe("1G-D2 backup controller: download and discard", () => {
  it("download links are validated and never fetched", async () => {
    const h = harness();
    await loaded(h, [ready()]);
    const e = h.c.current.exports![0]!;
    const now = 1_000_000;
    const deadline = h.c.current.deadlines[A];
    expect(downloadHref(e, deadline, now)).toBe(`/control/v1/admin/backup/exports/${A}/download`);
    for (const bad of [`http://evil.example/control/v1/admin/backup/exports/${A}/download`, `//evil/x`, exportDownloadPath(B), `${exportDownloadPath(A)}?x=1`]) {
      expect(downloadHref({ ...e, download: bad }, deadline, now), bad).toBeNull();
    }
    for (const state of ["preparing", "failed", "expired", "cleanup_pending"] as const) expect(downloadHref({ ...e, state }, deadline, now)).toBeNull();
    // Starting a download only re-reads the list (never the archive, never a dismissal).
    h.c.downloadStarted(A);
    expect(h.c.current.downloadStarted).toBe(A);
    expect(h.lists).toHaveLength(2);
    expect(h.dismissals).toHaveLength(0);
    h.lists[1]!.resolve([ready()]);
    await flush();
    expect(h.c.current.exports![0]!.state).toBe("ready"); // still downloadable again
  });

  it("discarding needs the explicit question first; a preparing export cannot be discarded", async () => {
    const h = harness();
    await loaded(h, [preparing(B), ready(A)]);
    await h.c.dismiss(A); // no question asked: nothing sent
    expect(h.dismissals).toHaveLength(0);
    h.c.askDismiss(B);
    expect(h.c.current.dismiss.confirming).toBeNull();
    h.c.askDismiss(A);
    expect(h.c.current.dismiss.confirming).toBe(A);
    h.c.keep();
    expect(h.c.current.dismiss.confirming).toBeNull();
    h.c.askDismiss(A);
    const a = h.c.dismiss(A);
    const b = h.c.dismiss(A);
    expect(h.dismissals).toHaveLength(1); // one DELETE
    expect(h.c.current.dismiss.busy).toBe(A);
    h.dismissals[0]!.d.resolve({ export_id: A, cleanup: "done" });
    await Promise.all([a, b]);
    expect(h.c.current.exports!.map((e) => e.export_id)).toEqual([B]);
    expect(h.c.current.dismiss.notice?.text).toMatch(/does not|not affected/);
    expect(h.c.current.dismiss.notice?.text).toMatch(/notebooks and any ZIP you saved are not affected/);
  });

  it("deferred cleanup, a failed cleanup, gone and unknown outcomes are explained accurately", async () => {
    const outcome = async (settle: (d: ReturnType<typeof deferred<BackupExportDismissed>>) => void) => {
      const h = harness();
      await loaded(h, [ready()]);
      h.c.askDismiss(A);
      const p = h.c.dismiss(A);
      settle(h.dismissals[0]!.d);
      await p;
      return h;
    };
    let h = await outcome((d) => d.resolve({ export_id: A, cleanup: "deferred" }));
    expect(h.c.current.dismiss.notice).toMatchObject({ tone: "warn" });
    expect(h.c.current.dismiss.notice!.text).toMatch(/withdrawn.*download already in progress may finish/);
    expect(h.lists).toHaveLength(2);
    h = await outcome((d) => d.reject(new ControlError(500, "cleanup_failed", "m")));
    expect(h.c.current.dismiss.notice!.text).toMatch(/withdrawn.*retries automatically/);
    h = await outcome((d) => d.reject(new ControlError(409, "export_preparing", "m")));
    expect(h.c.current.dismiss.notice!.text).toMatch(/still being prepared/);
    h = await outcome((d) => d.reject(new ControlError(404, "not_found", "m")));
    expect(h.c.current.dismiss.notice!.text).toMatch(/no longer has/);
    expect(h.lists).toHaveLength(2);
    h = await outcome((d) => d.reject(new ControlError(410, "expired", "m")));
    expect(h.c.current.dismiss.notice!.text).toMatch(/already expired/);
    h = await outcome((d) => d.reject(new ControlError(0, "interrupted", "m")));
    expect(h.c.current.dismiss.notice!.text).toMatch(/not known whether/);
    expect(h.c.current.exports).toHaveLength(1); // not assumed gone
    expect(h.lists).toHaveLength(2);
  });
});

// ------------------------------------------------------------------ the view

const HANDLERS: BackupExportHandlers = {
  onCreate: () => undefined,
  onRefresh: () => undefined,
  onDownload: () => undefined,
  onAskDismiss: () => undefined,
  onKeep: () => undefined,
  onDismiss: () => undefined,
};

const NOW = 5_000_000;
/** The Create button, disabled (React writes `disabled` before the test id). */
const CREATE_DISABLED = /<button[^>]*disabled=""[^>]*data-testid="backup-create"/;

function view(s: Partial<BackupExportsState>, now = NOW): string {
  return renderToStaticMarkup(<BackupExportsView state={{ ...INITIAL_EXPORTS_STATE, ...s }} now={now} handlers={HANDLERS} />);
}

describe("1G-D2 backup view", () => {
  it("empty: an explanation and an enabled Create backup; loading: nothing to create yet", () => {
    const html = view({ exports: [] });
    expect(html).toContain("Create a portable backup of the libraries currently stored on this Ignite Server.");
    expect(html).toMatch(/data-testid="backup-create"[^>]*>Create backup</);
    expect(html).not.toMatch(CREATE_DISABLED);
    expect(html).toContain("To restore a saved backup later, select its ZIP in Restore below.");
    expect(view({})).toMatch(CREATE_DISABLED);
  });

  it("preparing: an honest message, no percentage, no download, no discard, Create disabled", () => {
    const html = view({ exports: [preparing()] });
    expect(html).toContain("Preparing and verifying your backup…");
    expect(html).not.toMatch(/\d+\s?%/);
    expect(html).not.toContain("backup-download");
    expect(html).not.toContain("backup-discard");
    expect(html).toMatch(CREATE_DISABLED);
    expect(html).toContain("One backup is kept per administrator");
  });

  it("ready: the server's real facts, a full SHA-256, the remaining time and a validated native download link", () => {
    const e = ready();
    const html = view({ exports: [e], deadlines: { [A]: NOW + 14 * 60_000 } });
    for (const t of ["2", "18", "94", "3 (121 KB)", "1.2 MB", "1,234,567 bytes", "ignite-backup.zip", "ab".repeat(32), "about 14 minutes"]) expect(html).toContain(t);
    expect(html).toMatch(new RegExp(`<a class="btn primary" href="${exportDownloadPath(A)}" download="ignite-backup.zip" referrerPolicy="no-referrer"`, "i"));
    expect(html).toContain("Each library is captured at its own snapshot point. This is not one atomic snapshot across all libraries.");
    expect(html).toContain("not a full system backup");
    expect(html).toContain("operation history");
    expect(html).toContain("device credentials and pairings");
    expect(html).toContain("Discard prepared backup…");
    expect(html).not.toMatch(/Downloaded successfully|saved successfully/i);
  });

  it("a started download is not presented as a saved file", () => {
    const html = view({ exports: [ready()], deadlines: { [A]: NOW + 60_000 }, downloadStarted: A });
    expect(html).toContain("Download started in your browser");
    expect(html).toContain("make sure");
    expect(html).not.toMatch(/download(ed)? (complete|successful)/i);
    expect(html).toContain('data-testid="backup-download"'); // can be downloaded again
  });

  it("expiry: past the reported time the download is disabled until the server answers", () => {
    const html = view({ exports: [ready()], deadlines: { [A]: NOW - 1 } });
    expect(html).not.toContain('data-testid="backup-download"');
    expect(html).toContain('disabled="" data-testid="backup-download-disabled"');
    expect(html).toContain("Checking whether the backup expired");
    const expired = view({ exports: [{ ...ready(), state: "expired", expires_in_s: 0, download: null }] });
    expect(expired).toContain("Backup expired");
    expect(expired).toContain("Create a new backup");
    expect(expired).not.toContain('data-testid="backup-download"');
  });

  it("failed and cleanup-pending exports offer no download", () => {
    const f = view({ exports: [failed()] });
    expect(f).toContain("Backup failed");
    expect(f).toContain("io_error");
    expect(f).toContain("Nothing was offered for download");
    expect(f).toContain("Discard prepared backup…");
    expect(f).not.toContain('data-testid="backup-download"');
    expect(f).not.toMatch(CREATE_DISABLED); // a failed result does not block
    const c = view({ exports: [{ ...ready(), state: "cleanup_pending", download: null }] });
    expect(c).toContain("Cleanup pending");
    expect(c).not.toContain('data-testid="backup-download"');
    expect(c).not.toContain("backup-discard");
  });

  it("the discard question explains what is (and is not) removed", () => {
    const html = view({ exports: [ready()], deadlines: { [A]: NOW + 60_000 }, dismiss: { confirming: A, busy: null, notice: null } });
    expect(html).toContain("This removes the temporary backup from the server. It does not delete your notebooks or a ZIP already saved on your computer.");
    expect(html).toContain("Discard prepared backup");
    expect(html).toContain("Keep");
  });

  it("outcome notices: unknown, found, none, refused", () => {
    expect(view({ exports: [], create: { kind: "uncertain", checkFailed: true } })).toContain("Backup request outcome unknown.");
    expect(view({ exports: [], create: { kind: "uncertain", checkFailed: true } })).toContain("choose Refresh");
    expect(view({ exports: [preparing()], create: { kind: "uncertain_found" } })).toContain("It may be the one you requested.");
    expect(view({ exports: [], create: { kind: "uncertain_none" } })).toContain("reports no backup from that request");
    const refused = view({
      exports: [],
      create: { kind: "refused", error: { kind: "capacity", title: "You already have a backup", message: "<b>x</b>", code: "export_exists" } },
    });
    expect(refused).toContain("&lt;b&gt;x&lt;/b&gt;"); // server text is text
    expect(refused).toContain("export_exists");
  });

  it("server strings are escaped; refused access hides Create and every export", () => {
    const html = view({ exports: [{ ...failed(), error: { code: "<i>c</i>", message: "<script>alert(1)</script>" } }] });
    expect(html).not.toContain("<script>");
    expect(html).toContain("&lt;script&gt;");
    const refused = view({ exports: null, error: { kind: "loopback", message: BACKUP_LOOPBACK_EXPLANATION } });
    expect(refused).toContain("secure tunnel");
    expect(refused).not.toContain("backup-create");
    expect(refused).not.toContain('data-testid="backup-export"');
  });
});

describe("1G-D2 placement in Backup & Restore", () => {
  const restoreState = { jobs: [], loading: false, error: null };
  it("without the backup half, the restore section renders exactly as before", () => {
    const html = renderToStaticMarkup(<RestoreJobsView state={restoreState} expanded={new Set()} onToggle={() => undefined} onRefresh={() => undefined} />);
    expect(html).toMatch(/<h2 id="restore-heading">Backup &amp; Restore<\/h2><button class="btn" data-testid="restore-refresh">Refresh<\/button>/);
    expect(html).not.toContain("<h3>Restore</h3>");
  });

  it("with it: Backup first, then Restore with its own Refresh; restore controls unchanged", () => {
    const html = renderToStaticMarkup(
      <RestoreJobsView state={restoreState} expanded={new Set()} onToggle={() => undefined} onRefresh={() => undefined} backup={<div data-testid="backup-slot" />} />,
    );
    const backupAt = html.indexOf("backup-slot");
    const restoreAt = html.indexOf("<h3>Restore</h3>");
    expect(backupAt).toBeGreaterThan(html.indexOf("Backup &amp; Restore"));
    expect(restoreAt).toBeGreaterThan(backupAt);
    expect(html.indexOf('data-testid="restore-refresh"')).toBeGreaterThan(restoreAt);
    expect(html).toContain("No restore jobs yet.");
  });
});

// ------------------------------------------------------------------ 1G-D2 corrective pass

describe("1G-D2 corrective: a refusal is final for everything started before it", () => {
  for (const [status, code, kind] of [
    [401, "unauthorized", "session"],
    [403, "loopback_required", "loopback"],
  ] as const) {
    it(`a Create POST answered 202 after a list was refused (${status} ${code}) brings nothing back`, async () => {
      const h = harness();
      await loaded(h, []); // 1. an initial, successful, empty list
      const post = h.c.create(); // 2. the POST stays unanswered
      expect(h.starts).toHaveLength(1);
      const r = h.c.refresh(); // 3. a concurrent list is refused
      h.lists[1]!.reject(new ControlError(status, code, "server text"));
      await r;
      // 4. cleared at once
      expect(h.c.current.exports).toBeNull();
      expect(h.c.current.error?.kind).toBe(kind);
      const settled = h.lists.length;
      h.starts[0]!.resolve(preparing()); // 5. the old POST is accepted after all
      await post;
      await flush();
      // 6./7. nothing reappears; the refusal stays visible
      expect(h.c.current.exports).toBeNull();
      expect(h.c.current.deadlines).toEqual({});
      expect(h.c.current.error?.kind).toBe(kind);
      expect(h.c.current.create.kind).not.toBe("accepted");
      expect(canCreate(h.c.current)).toBe(false);
      // 8. nothing follows on its own: no retry, no mutation, no list, no polling
      await vi.advanceTimersByTimeAsync(10 * PREPARING_POLL_MS);
      expect(h.starts).toHaveLength(1);
      expect(h.dismissals).toHaveLength(0);
      expect(h.lists).toHaveLength(settled);
      // Not locked: a fresh list the server answers restores it under the usual rules.
      const again = h.c.refresh();
      h.lists.at(-1)!.resolve([preparing()]);
      await again;
      expect(h.c.current.exports).toEqual([preparing()]);
      expect(h.c.current.error).toBeNull();
    });
  }

  it("a late failure of that POST is not reported over the refusal either", async () => {
    const h = harness();
    await loaded(h, []);
    const post = h.c.create();
    const r = h.c.refresh();
    h.lists[1]!.reject(new ControlError(401, "unauthorized", "not signed in"));
    await r;
    h.starts[0]!.reject(new ControlError(0, "interrupted", "x")); // would otherwise be "uncertain"
    await post;
    await flush();
    expect(h.c.current.exports).toBeNull();
    expect(h.c.current.create.kind).toBe("idle");
    expect(h.lists).toHaveLength(2); // no check started for it
  });

  it("a DELETE answered after a refusal changes nothing", async () => {
    const h = harness();
    await loaded(h, [ready()]);
    h.c.askDismiss(A);
    const d = h.c.dismiss(A);
    const r = h.c.refresh();
    h.lists[1]!.reject(new ControlError(403, "forbidden", "administrator access required"));
    await r;
    h.dismissals[0]!.d.resolve({ export_id: A, cleanup: "done" });
    await d;
    await flush();
    expect(h.c.current.exports).toBeNull(); // not an (empty) list
    expect(h.c.current.error?.kind).toBe("access");
    expect(h.lists).toHaveLength(2);
  });

  it("ordinary creation is unchanged", async () => {
    const h = harness();
    await loaded(h, []);
    const p = h.c.create();
    h.starts[0]!.resolve(preparing());
    await p;
    expect(h.c.current.exports).toEqual([preparing()]);
    expect(h.c.current.create).toEqual({ kind: "accepted", exportId: A });
    expect(h.lists).toHaveLength(2);
  });
});

describe("1G-D2 corrective: a definitive withdrawal removes Download at once", () => {
  const withdrawnBy = async (settle: (d: ReturnType<typeof deferred<BackupExportDismissed>>) => void) => {
    const h = harness();
    await loaded(h, [ready()]);
    // 1. ready, Download offered
    expect(downloadHref(h.c.current.exports![0]!, h.c.current.deadlines[A], 1_000_000)).toBe(exportDownloadPath(A));
    expect(view(h.c.current, 1_000_000)).toContain('data-testid="backup-download"');
    h.c.askDismiss(A);
    const p = h.c.dismiss(A);
    settle(h.dismissals[0]!.d);
    await p;
    return h;
  };

  const noDownload = (s: BackupExportsState) => {
    const e = s.exports!.find((x) => x.export_id === A)!;
    expect(e.state).toBe("cleanup_pending");
    expect(e.download).toBeNull();
    expect(downloadHref(e, s.deadlines[A], 1_000_000)).toBeNull();
    const html = view(s, 1_000_000);
    expect(html).not.toContain('data-testid="backup-download"');
    expect(html).not.toContain("backup-download-disabled");
    expect(html).toContain("Cleanup pending");
    expect(html).toContain("still removing its temporary file"); // awaiting deletion, not deleted
    expect(html).not.toContain("backup-discard\"");
  };

  for (const [what, settle] of [
    ["200 with deferred cleanup", (d: ReturnType<typeof deferred<BackupExportDismissed>>) => d.resolve({ export_id: A, cleanup: "deferred" })],
    ["500 cleanup_failed", (d: ReturnType<typeof deferred<BackupExportDismissed>>) => d.reject(new ControlError(500, "cleanup_failed", "m"))],
  ] as const) {
    it(`${what}: gone before the next list, stays gone if it fails, the next good list decides`, async () => {
      const h = await withdrawnBy(settle);
      // 3. the follow-up list is not answered yet
      expect(h.lists).toHaveLength(2);
      noDownload(h.c.current);
      expect(h.c.current.dismiss.notice!.text).toMatch(/withdrawn/);
      // 4. it fails: still no Download
      h.lists[1]!.reject(new ControlError(0, "unreachable", "Ignite Server is not reachable."));
      await flush();
      expect(h.c.current.error?.kind).toBe("transient");
      noDownload(h.c.current);
      // 5. a later list reports it cleanup pending, then gone
      let r = h.c.refresh();
      h.lists[2]!.resolve([{ ...ready(), state: "cleanup_pending", download: null }]);
      await r;
      noDownload(h.c.current);
      r = h.c.refresh();
      h.lists[3]!.resolve([]);
      await r;
      expect(h.c.current.exports).toEqual([]);
    });

    it(`${what}: a list requested before the DELETE cannot bring Download back`, async () => {
      const h = harness();
      await loaded(h, [ready()]);
      const old = h.c.refresh();
      h.c.askDismiss(A);
      const p = h.c.dismiss(A);
      settle(h.dismissals[0]!.d);
      await p;
      h.lists[1]!.resolve([ready()]); // answered for the moment before the withdrawal
      await old;
      await flush();
      noDownload(h.c.current);
    });
  }

  it("a fully deleted backup is removed, not shown as awaiting cleanup", async () => {
    const h = await withdrawnBy((d) => d.resolve({ export_id: A, cleanup: "done" }));
    expect(h.c.current.exports).toEqual([]);
    expect(h.c.current.deadlines).toEqual({});
  });

  it("a lost DELETE answer stays uncertain: nothing is assumed withdrawn", async () => {
    const h = await withdrawnBy((d) => d.reject(new ControlError(0, "interrupted", "x")));
    const e = h.c.current.exports![0]!;
    expect(e.state).toBe("ready");
    expect(h.c.current.dismiss.notice!.text).toMatch(/not known whether/);
    expect(h.lists).toHaveLength(2); // the server is asked
    h.lists[1]!.resolve([{ ...ready(), state: "cleanup_pending", download: null }]);
    await flush();
    expect(h.c.current.exports![0]!.state).toBe("cleanup_pending"); // its answer decides
  });
});
