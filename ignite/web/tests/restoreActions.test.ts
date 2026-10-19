// Slice 1G-B: the upload/cancel controller. The one-time confirmation must never appear in its
// public state, and must be dropped on cancel, expiry, unusability, lost access and teardown.

import { afterEach, describe, expect, it, vi } from "vitest";
import { ControlError, MAX_RESTORE_UPLOAD_BYTES, type PreparedRestore, type RestoreJob } from "../src/api/controlApi";
import { RestoreActions, type ActionsState } from "../src/components/restoreActions";
import { RestoreJobsLoader, type RestoreErrorKind, type RestoreJobsState } from "../src/components/restoreJobsLoader";

const SECRET = "S3cr3tS3cr3tS3cr3tS3cr3tS3cr3tS3cr3tS3cr3t1"; // 43 base64url characters

function job(over: Partial<RestoreJob> = {}): RestoreJob {
  return {
    job_id: "job-1",
    state: "ready",
    usable: true,
    cleanup_pending: false,
    expires_in_s: 900,
    archive: { sha256: "ab".repeat(32), bytes: 4096, format: "ignite-portable-backup", format_version: 1, snapshot_version: 1 },
    scope: "current_state_only",
    not_restored: [],
    totals: { libraries: 2, notebooks: 5, pages: 9, blobs: 1, blob_bytes: 1200 },
    libraries: [],
    warnings: [],
    result: null,
    ...over,
  };
}

/** A promise the test settles by hand. */
function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((ok, fail) => {
    resolve = ok;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function setup() {
  const uploads: { file: Blob; signal?: AbortSignal; d: ReturnType<typeof deferred<PreparedRestore>> }[] = [];
  const cancels: { id: string; d: ReturnType<typeof deferred<void>> }[] = [];
  const states: ActionsState[] = [];
  const prepared: RestoreJob[] = [];
  let refreshes = 0;
  const refusals: RestoreErrorKind[] = [];
  const a = new RestoreActions(
    {
      uploadRestore: (file, signal) => {
        const d = deferred<PreparedRestore>();
        uploads.push({ file, signal, d });
        return d.promise;
      },
      cancelRestore: (id) => {
        const d = deferred<void>();
        cancels.push({ id, d });
        return d.promise;
      },
    },
    {
      onChange: (s) => states.push(s),
      onPrepared: (j) => prepared.push(j),
      refresh: () => void refreshes++,
      accessRefused: (e) => void refusals.push(e.kind),
    },
  );
  return { a, uploads, cancels, states, prepared, refusals, refreshes: () => refreshes };
}

const zip = (name = "ignite-backup.zip", bytes = 10) => new File([new Uint8Array(bytes)], name, { type: "application/zip" });

/** Everything the UI could ever see from the controller. */
const visible = (s: { states: ActionsState[]; a: RestoreActions }) => JSON.stringify([s.states, s.a.current]);

afterEach(() => vi.useRealTimers());

describe("1G-B restore actions", () => {
  it("nothing selected: upload does nothing", async () => {
    const s = setup();
    await s.a.upload();
    expect(s.uploads).toHaveLength(0);
    expect(s.a.current.upload.kind).toBe("idle");
  });

  it("empty and oversized files are refused locally, without allocating them", async () => {
    const s = setup();
    s.a.select(zip("empty.zip", 0));
    expect(s.a.current.upload).toMatchObject({ kind: "invalid", message: expect.stringContaining("empty") });
    // A size-only stand-in for a > 2 GiB file (no multi-gigabyte allocation).
    s.a.select({ name: "huge.zip", type: "application/zip", size: MAX_RESTORE_UPLOAD_BYTES + 1 } as File);
    expect(s.a.current.upload).toMatchObject({ kind: "invalid", message: expect.stringContaining("2 GB") });
    await s.a.upload();
    expect(s.uploads).toHaveLength(0);
    s.a.select({ name: "edge.zip", type: "application/zip", size: MAX_RESTORE_UPLOAD_BYTES } as File);
    expect(s.a.current.upload.kind).toBe("selected");
  });

  it("a non-ZIP name only warns (the server decides)", () => {
    const s = setup();
    s.a.select(new File(["x"], "notes.txt", { type: "text/plain" }));
    expect(s.a.current.upload).toMatchObject({ kind: "selected", warning: expect.stringContaining("ZIP") });
    s.a.select(zip());
    expect(s.a.current.upload).toMatchObject({ kind: "selected", warning: null });
  });

  it("one upload at a time: double clicks send one request; success only after the verified 201", async () => {
    const s = setup();
    const file = zip();
    s.a.select(file);
    const first = s.a.upload();
    void s.a.upload();
    void s.a.upload();
    s.a.select(zip("other.zip")); // ignored while uploading
    expect(s.uploads).toHaveLength(1);
    expect(s.uploads[0]!.file).toBe(file); // the File itself
    expect(s.a.current.upload).toMatchObject({ kind: "uploading", name: "ignite-backup.zip" });
    expect(s.prepared).toHaveLength(0);
    s.uploads[0]!.d.resolve({ job: job(), confirmation: SECRET });
    await first;
    expect(s.a.current.upload).toMatchObject({ kind: "prepared", job: { job_id: "job-1" } });
    expect(s.prepared.map((j) => j.job_id)).toEqual(["job-1"]);
    expect(s.a.holdsConfirmation("job-1")).toBe(true);
    expect(visible(s)).not.toContain(SECRET);
    expect(s.a.current.picker).toBe(1); // the file input is reset after the upload
  });

  it("a refused backup is a definite failure; an interrupted one is uncertain and prompts a refresh", async () => {
    const s = setup();
    s.a.select(zip());
    let p = s.a.upload();
    s.uploads[0]!.d.reject(new ControlError(422, "archive_invalid", "not a ZIP archive", "zip_not_zip"));
    await p;
    expect(s.a.current.upload).toMatchObject({ kind: "failed", error: { kind: "rejected", code: "archive_invalid: zip_not_zip" } });
    expect(s.refreshes()).toBe(0);
    s.a.select(zip());
    p = s.a.upload();
    s.uploads[1]!.d.reject(new ControlError(0, "upload_interrupted", "interrupted"));
    await p;
    const failed = s.a.current.upload;
    expect(failed).toMatchObject({ kind: "failed", error: { kind: "uncertain" } });
    if (failed.kind === "failed") expect(failed.error.message).toMatch(/not known whether the backup was received.*Refresh the list/s);
    expect(s.refreshes()).toBe(1);
    expect(s.uploads).toHaveLength(2); // never retried automatically
    expect(s.a.current.picker).toBe(2); // reset after each finished attempt, failed ones too
  });

  it("capacity, size, storage and session refusals are explained", async () => {
    const cases: [ControlError, string, RegExp][] = [
      [new ControlError(429, "too_many_jobs", "x"), "capacity", /one prepared restore per administrator/],
      [new ControlError(429, "upload_busy", "x"), "capacity", /one backup at a time/],
      [new ControlError(413, "body_too_large", "x"), "rejected", /upload limit/],
      [new ControlError(507, "insufficient_storage", "x"), "storage", /free space/],
      [new ControlError(401, "unauthorized", "x"), "session", /no longer valid/],
      [new ControlError(403, "loopback_required", "x"), "loopback", /server itself/],
      [new ControlError(403, "csrf", "missing marker"), "access", /missing marker/],
    ];
    for (const [e, kind, text] of cases) {
      const s = setup();
      s.a.select(zip());
      const p = s.a.upload();
      s.uploads[0]!.d.reject(e);
      await p;
      const u = s.a.current.upload;
      expect(u.kind).toBe("failed");
      if (u.kind === "failed") {
        expect(u.error.kind, e.code).toBe(kind);
        expect(u.error.message, e.code).toMatch(text);
      }
    }
  });

  it("cancel: asks first, sends one DELETE, then drops the confirmation and refreshes", async () => {
    const s = setup();
    s.a.select(zip());
    const p = s.a.upload();
    s.uploads[0]!.d.resolve({ job: job(), confirmation: SECRET });
    await p;
    s.a.askCancel("job-1");
    expect(s.a.current.cancel.confirming).toBe("job-1");
    s.a.keep();
    expect(s.a.current.cancel.confirming).toBeNull();
    s.a.askCancel("job-1");
    const c = s.a.cancel("job-1");
    void s.a.cancel("job-1"); // double click
    expect(s.cancels).toHaveLength(1);
    expect(s.a.current.cancel.busy).toBe("job-1");
    s.cancels[0]!.d.resolve();
    await c;
    expect(s.a.holdsConfirmation("job-1")).toBe(false);
    expect(s.a.current.cancel.notice).toMatchObject({ tone: "ok", text: expect.stringContaining("Nothing was restored") });
    expect(s.a.current.upload.kind).toBe("idle");
    expect(s.refreshes()).toBe(1);
    expect(visible(s)).not.toContain(SECRET);
  });

  it("cancel refusals: executing, expired, cleanup pending and an unanswered request", async () => {
    const run = async (e: unknown) => {
      const s = setup();
      s.a.select(zip());
      const p = s.a.upload();
      s.uploads[0]!.d.resolve({ job: job(), confirmation: SECRET });
      await p;
      const c = s.a.cancel("job-1");
      s.cancels[0]!.d.reject(e);
      await c;
      return { notice: s.a.current.cancel.notice!, holds: s.a.holdsConfirmation("job-1"), refreshes: s.refreshes() };
    };
    let r = await run(new ControlError(409, "job_executing", "running"));
    expect([r.notice.tone, r.holds]).toEqual(["bad", false]); // executing: never confirmable again
    expect(r.notice.text).toMatch(/can no longer be cancelled/);
    r = await run(new ControlError(410, "expired", "expired"));
    expect([r.notice.tone, r.holds]).toEqual(["warn", false]);
    expect(r.notice.text).toMatch(/already been cancelled or had expired/);
    r = await run(new ControlError(500, "cleanup_failed", "retrying"));
    expect(r.holds).toBe(false);
    expect(r.notice.text).toMatch(/withdrawn.*retrying/s);
    r = await run(new ControlError(0, "unreachable", "down"));
    expect(r.holds).toBe(true); // not assumed cancelled: the refreshed list decides
    expect(r.notice.text).toMatch(/not known whether the restore was cancelled/);
    expect(r.refreshes).toBe(1);
  });

  it("the confirmation follows the server: dropped when unusable, gone, or access is lost", async () => {
    const prep = async () => {
      const s = setup();
      s.a.select(zip());
      const p = s.a.upload();
      s.uploads[0]!.d.resolve({ job: job(), confirmation: SECRET });
      await p;
      return s;
    };
    let s = await prep();
    s.a.reconcile([job()], null);
    expect(s.a.holdsConfirmation("job-1")).toBe(true);
    s.a.reconcile(null, "transient"); // a transient failure keeps it
    expect(s.a.holdsConfirmation("job-1")).toBe(true);
    s.a.reconcile([job({ state: "expired", usable: false })], null);
    expect(s.a.holdsConfirmation("job-1")).toBe(false);
    expect(s.a.current.upload.kind).toBe("idle");
    s = await prep();
    s.a.reconcile([], null);
    expect(s.a.holdsConfirmation("job-1")).toBe(false);
    for (const kind of ["session", "loopback", "access"] as const) {
      s = await prep();
      s.a.reconcile(null, kind);
      expect(s.a.holdsConfirmation("job-1"), kind).toBe(false);
    }
  });

  it("the confirmation expires with the prepared job", async () => {
    vi.useFakeTimers();
    const s = setup();
    s.a.select(zip());
    const p = s.a.upload();
    s.uploads[0]!.d.resolve({ job: job({ expires_in_s: 60 }), confirmation: SECRET });
    await p;
    vi.advanceTimersByTime(59_000);
    expect(s.a.holdsConfirmation("job-1")).toBe(true);
    vi.advanceTimersByTime(1_000);
    expect(s.a.holdsConfirmation("job-1")).toBe(false);
    expect(s.refreshes()).toBe(1);
  });

  it("teardown aborts an upload in flight, forgets the confirmation and reports nothing", async () => {
    const s = setup();
    s.a.select(zip());
    const p = s.a.upload();
    const signal = s.uploads[0]!.signal!;
    const seen = s.states.length;
    s.a.dispose();
    expect(signal.aborted).toBe(true);
    s.uploads[0]!.d.resolve({ job: job(), confirmation: SECRET }); // too late
    await p;
    expect(s.states).toHaveLength(seen);
    expect(s.prepared).toHaveLength(0);
    expect(s.a.holdsConfirmation("job-1")).toBe(false);

    const t = setup();
    t.a.select(zip());
    const q = t.a.upload();
    t.uploads[0]!.d.resolve({ job: job(), confirmation: SECRET });
    await q;
    expect(t.a.holdsConfirmation("job-1")).toBe(true);
    t.a.dispose();
    expect(t.a.holdsConfirmation("job-1")).toBe(false);
  });
});

// ------------------------------------------------------------------ 1G-B final corrections

describe("1G-B corrections: a settled upload releases its File", () => {
  for (const [name, settle] of [
    ["success", (d: ReturnType<typeof deferred<PreparedRestore>>) => d.resolve({ job: job(), confirmation: SECRET })],
    ["rejection", (d: ReturnType<typeof deferred<PreparedRestore>>) => d.reject(new ControlError(422, "archive_invalid", "bad", "zip_not_zip"))],
    ["interruption", (d: ReturnType<typeof deferred<PreparedRestore>>) => d.reject(new ControlError(0, "upload_interrupted", "lost"))],
    ["server error", (d: ReturnType<typeof deferred<PreparedRestore>>) => d.reject(new ControlError(500, "internal", "boom"))],
  ] as const) {
    it(`after ${name}, uploading again needs a new selection; the same file can be chosen again`, async () => {
      const s = setup();
      const file = zip();
      s.a.select(file);
      const p = s.a.upload();
      void s.a.upload(); // double click while in flight
      expect(s.uploads).toHaveLength(1);
      expect(s.uploads[0]!.file).toBe(file); // held while in flight
      settle(s.uploads[0]!.d);
      await p;
      await s.a.upload();
      await s.a.upload();
      expect(s.uploads).toHaveLength(1); // nothing sent without a new select()
      s.a.select(file); // the same File again
      expect(s.a.current.upload.kind).toBe("selected");
      void s.a.upload();
      expect(s.uploads).toHaveLength(2);
      expect(s.uploads[1]!.file).toBe(file);
    });
  }
});

describe("1G-B corrections: cancellation 409 drops only that job's capability", () => {
  it("409 drops the matching confirmation; another job's is untouched; no answer keeps it", async () => {
    const s = setup();
    s.a.select(zip());
    const p = s.a.upload();
    s.uploads[0]!.d.resolve({ job: job({ job_id: "mine" }), confirmation: SECRET });
    await p;
    // A 409 for some other job leaves this capability alone.
    let c = s.a.cancel("other");
    s.cancels[0]!.d.reject(new ControlError(409, "job_executing", "running"));
    await c;
    expect(s.a.holdsConfirmation("mine")).toBe(true);
    expect(s.a.current.upload.kind).toBe("prepared");
    // An unanswered DELETE is not a cancellation.
    c = s.a.cancel("mine");
    s.cancels[1]!.d.reject(new ControlError(0, "unreachable", "down"));
    await c;
    expect(s.a.holdsConfirmation("mine")).toBe(true);
    expect(s.a.current.cancel.notice?.text).toMatch(/not known whether the restore was cancelled/);
    expect(s.a.current.cancel.notice?.text).not.toMatch(/discarded/);
    // 409 for this job: executing, so never confirmable again.
    c = s.a.cancel("mine");
    s.cancels[2]!.d.reject(new ControlError(409, "job_executing", "running"));
    await c;
    expect(s.a.holdsConfirmation("mine")).toBe(false);
    expect(s.a.current.cancel.notice).toMatchObject({ jobId: "mine", tone: "bad", text: expect.stringContaining("can no longer be cancelled") });
    expect(s.refreshes()).toBe(3); // every outcome re-reads the list
    expect(visible(s)).not.toContain(SECRET);
  });
});

/** The section's wiring (as RestoreJobs.tsx does it) around hand-settled GETs and actions. */
function wired() {
  const gets: ReturnType<typeof deferred<RestoreJob[]>>[] = [];
  let listState: RestoreJobsState = { jobs: null, loading: false, error: null };
  const loader = new RestoreJobsLoader(
    () => {
      const d = deferred<RestoreJob[]>();
      gets.push(d);
      return d.promise;
    },
    (s) => (listState = s),
  );
  const uploads: ReturnType<typeof deferred<PreparedRestore>>[] = [];
  const cancels: ReturnType<typeof deferred<void>>[] = [];
  const a = new RestoreActions(
    {
      uploadRestore: () => {
        const d = deferred<PreparedRestore>();
        uploads.push(d);
        return d.promise;
      },
      cancelRestore: () => {
        const d = deferred<void>();
        cancels.push(d);
        return d.promise;
      },
    },
    {
      onChange: () => undefined,
      onPrepared: (j) => {
        loader.upsert(j);
        void loader.refreshAfter();
      },
      refresh: () => void loader.refreshAfter(),
      accessRefused: (e) => loader.refuse(e),
    },
  );
  return { loader, a, gets, uploads, cancels, list: () => listState };
}

const settle = async () => {
  for (let i = 0; i < 5; i++) await Promise.resolve();
};

describe("1G-B corrections: an action's access refusal drops privileged job data at once", () => {
  it("upload 401: cleared immediately; an earlier GET that succeeds cannot bring it back", async () => {
    const w = wired();
    let r = w.loader.refresh();
    w.gets[0]!.resolve([job({ job_id: "a" })]);
    await r;
    expect(w.list().jobs?.map((j) => j.job_id)).toEqual(["a"]);
    r = w.loader.refresh(); // an older GET, deliberately left unresolved
    w.a.select(zip());
    const u = w.a.upload();
    w.uploads[0]!.reject(new ControlError(401, "unauthorized", "not signed in"));
    await u;
    expect(w.list().jobs).toBeNull(); // immediately, while the GET is still pending
    expect(w.list().error?.kind).toBe("session");
    w.gets[1]!.resolve([job({ job_id: "a" })]); // the earlier GET answers successfully
    await r;
    await settle();
    expect(w.list().jobs).toBeNull(); // not resurrected
    expect(w.list().error?.kind).toBe("session");
    // A later GET the server answers (a valid session again) restores the list as usual.
    expect(w.gets).toHaveLength(3);
    w.gets[2]!.resolve([job({ job_id: "a" })]);
    await settle();
    expect(w.list()).toMatchObject({ jobs: [{ job_id: "a" }], error: null });
  });

  it("cancel 403 loopback: cleared immediately with the loopback explanation; the old GET is discarded", async () => {
    const w = wired();
    let r = w.loader.refresh();
    w.gets[0]!.resolve([job({ job_id: "a" })]);
    await r;
    r = w.loader.refresh(); // unresolved
    const c = w.a.cancel("a");
    w.cancels[0]!.reject(new ControlError(403, "loopback_required", "only on the server"));
    await c;
    expect(w.list().jobs).toBeNull();
    expect(w.list().error?.kind).toBe("loopback");
    w.gets[1]!.resolve([job({ job_id: "a" })]);
    await r;
    await settle();
    expect(w.list().jobs).toBeNull();
    // The follow-up GET is refused too: the list stays empty.
    w.gets[2]!.reject(new ControlError(403, "loopback_required", "only on the server"));
    await settle();
    expect(w.list()).toMatchObject({ jobs: null, error: { kind: "loopback" } });
  });

  it("a transient action failure never erases the last valid list", async () => {
    const w = wired();
    const r = w.loader.refresh();
    w.gets[0]!.resolve([job({ job_id: "a" })]);
    await r;
    w.a.select(zip());
    const u = w.a.upload();
    w.uploads[0]!.reject(new ControlError(0, "upload_interrupted", "lost"));
    await u;
    expect(w.list().jobs?.map((j) => j.job_id)).toEqual(["a"]);
    const c = w.a.cancel("a");
    w.cancels[0]!.reject(new ControlError(0, "unreachable", "down"));
    await c;
    expect(w.list().jobs?.map((j) => j.job_id)).toEqual(["a"]);
    expect(w.list().error).toBeNull();
  });

  it("after a cancellation 409 the refreshed list shows the job as the server reports it", async () => {
    const w = wired();
    w.a.select(zip());
    const u = w.a.upload();
    w.uploads[0]!.resolve({ job: job({ job_id: "a" }), confirmation: SECRET });
    await u;
    expect(w.list().jobs?.[0]?.state).toBe("ready"); // the verified job, shown at once
    w.gets[0]!.resolve([job({ job_id: "a" })]);
    await settle();
    const c = w.a.cancel("a");
    w.cancels[0]!.reject(new ControlError(409, "job_executing", "running"));
    await c;
    expect(w.a.holdsConfirmation("a")).toBe(false);
    w.gets[1]!.resolve([job({ job_id: "a", state: "executing", usable: false })]);
    await settle();
    expect(w.list().jobs?.map((j) => [j.job_id, j.state])).toEqual([["a", "executing"]]);
  });
});
