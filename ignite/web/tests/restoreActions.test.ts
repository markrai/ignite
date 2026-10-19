// Slice 1G-B: the upload/cancel controller. The one-time confirmation must never appear in its
// public state, and must be dropped on cancel, expiry, unusability, lost access and teardown.

import { afterEach, describe, expect, it, vi } from "vitest";
import { ControlError, MAX_RESTORE_UPLOAD_BYTES, type PreparedRestore, type RestoreAccepted, type RestoreConfirmation, type RestoreJob } from "../src/api/controlApi";
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
  const confirms: { id: string; body: RestoreConfirmation; d: ReturnType<typeof deferred<RestoreAccepted>> }[] = [];
  let rechecks = 0;
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
      confirmRestore: (id, body) => {
        const d = deferred<RestoreAccepted>();
        confirms.push({ id, body, d });
        return d.promise;
      },
    },
    {
      recheck: () => ++rechecks,
      onChange: (s) => states.push(s),
      onPrepared: (j) => prepared.push(j),
      refresh: () => void refreshes++,
      accessRefused: (e) => void refusals.push(e.kind),
    },
  );
  return { a, uploads, cancels, confirms, states, prepared, refusals, refreshes: () => refreshes, rechecks: () => rechecks };
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
  const confirms: ReturnType<typeof deferred<RestoreAccepted>>[] = [];
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
      confirmRestore: () => {
        const d = deferred<RestoreAccepted>();
        confirms.push(d);
        return d.promise;
      },
    },
    {
      recheck: () => {
        const v = loader.invalidate();
        void loader.refreshAfter();
        return v;
      },
      onChange: () => undefined,
      onPrepared: (j) => {
        loader.upsert(j);
        void loader.refreshAfter();
      },
      refresh: () => void loader.refreshAfter(),
      accessRefused: (e) => loader.refuse(e),
    },
  );
  return { loader, a, gets, uploads, cancels, confirms, list: () => listState };
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

// ------------------------------------------------------------------ Slice 1G-C: confirmation

const MAPPED = job({
  job_id: "job-1",
  libraries: [
    { source_library_id: "src-1", destination_library_id: "dst-1", source_snapshot_sha256: "c".repeat(64), source_snapshot_bytes: 1, source_through_server_seq: 1, notebook_count: 1, page_count: 1, blob_count: 0 },
    { source_library_id: "src-2", destination_library_id: "dst-2", source_snapshot_sha256: "d".repeat(64), source_snapshot_bytes: 1, source_through_server_seq: 1, notebook_count: 1, page_count: 1, blob_count: 0 },
  ],
});

/** A controller holding the confirmation of MAPPED, with the final review open and acknowledged. */
async function reviewed() {
  const s = setup();
  s.a.select(zip());
  const p = s.a.upload();
  s.uploads[0]!.d.resolve({ job: MAPPED, confirmation: SECRET });
  await p;
  expect(s.a.current.confirmable).toBe("job-1");
  s.a.review("job-1");
  s.a.acknowledge("job-1", true);
  return s;
}

describe("1G-C confirmation", () => {
  it("opening and backing out of the review sends nothing; the final action needs the acknowledgement", async () => {
    const s = setup();
    s.a.select(zip());
    const p = s.a.upload();
    s.uploads[0]!.d.resolve({ job: MAPPED, confirmation: SECRET });
    await p;
    s.a.review("job-1");
    expect(s.a.current.confirm).toEqual({ kind: "reviewing", jobId: "job-1", acknowledged: false });
    await s.a.confirm("job-1"); // not acknowledged
    expect(s.confirms).toHaveLength(0);
    s.a.back();
    expect(s.a.current.confirm.kind).toBe("idle");
    await s.a.confirm("job-1"); // not reviewing
    expect(s.confirms).toHaveLength(0);
    s.a.review("other"); // not confirmable
    expect(s.a.current.confirm.kind).toBe("idle");
  });

  it("the explicit action sends exactly one request, built from the plan held since the 201", async () => {
    const s = await reviewed();
    const c = s.a.confirm("job-1");
    void s.a.confirm("job-1");
    void s.a.confirm("job-1");
    expect(s.confirms).toHaveLength(1);
    expect(s.confirms[0]!.id).toBe("job-1");
    expect(s.confirms[0]!.body).toEqual({
      v: 1,
      confirmation: SECRET,
      archive_sha256: MAPPED.archive.sha256,
      destinations: [
        { source_library_id: "src-1", destination_library_id: "dst-1" },
        { source_library_id: "src-2", destination_library_id: "dst-2" },
      ],
    });
    expect(s.a.current.confirm.kind).toBe("sending");
    s.a.askCancel("job-1");
    void s.a.cancel("job-1");
    expect(s.cancels).toHaveLength(0); // no cancelling while confirming
    s.confirms[0]!.d.resolve({ job_id: "job-1", state: "executing" });
    await c;
    expect(s.a.current.confirm).toEqual({ kind: "accepted", jobId: "job-1" });
    expect(s.a.holdsConfirmation("job-1")).toBe(false); // consumed
    expect(s.a.current.confirmable).toBeNull();
    expect(s.rechecks()).toBe(1);
    void s.a.cancel("job-1");
    expect(s.cancels).toHaveLength(0); // accepted: no Cancel
    s.a.review("job-1");
    expect(s.a.current.confirm.kind).toBe("accepted");
    expect(visible(s)).not.toContain(SECRET);
  });

  it("a server summary that differs from the approved plan discards the capability", async () => {
    for (const changed of [
      job({ ...MAPPED, archive: { ...MAPPED.archive, sha256: "ff".repeat(32) } }),
      job({ ...MAPPED, libraries: [MAPPED.libraries[0]!, { ...MAPPED.libraries[1]!, destination_library_id: "elsewhere" }] }),
      job({ ...MAPPED, libraries: [MAPPED.libraries[0]!] }),
    ]) {
      const s = await reviewed();
      s.a.reconcile([changed], null);
      expect(s.a.holdsConfirmation("job-1")).toBe(false);
      expect(s.a.current.confirmable).toBeNull();
      expect(s.a.current.confirm).toMatchObject({ kind: "refused", error: { title: "Can no longer be confirmed" } });
      await s.a.confirm("job-1");
      expect(s.confirms).toHaveLength(0);
    }
    // The same plan in another order stays confirmable.
    const s = await reviewed();
    s.a.reconcile([job({ ...MAPPED, libraries: [...MAPPED.libraries].reverse() })], null);
    expect(s.a.current.confirmable).toBe("job-1");
  });

  it("expiry during the review prevents submission", async () => {
    vi.useFakeTimers();
    const s = setup();
    s.a.select(zip());
    const p = s.a.upload();
    s.uploads[0]!.d.resolve({ job: { ...MAPPED, expires_in_s: 60 }, confirmation: SECRET });
    await p;
    s.a.review("job-1");
    s.a.acknowledge("job-1", true);
    vi.advanceTimersByTime(60_000);
    expect(s.a.current.confirm).toMatchObject({ kind: "refused", error: { title: "Expired" } });
    await s.a.confirm("job-1");
    expect(s.confirms).toHaveLength(0);
  });

  it("429 keeps the capability and needs another explicit attempt", async () => {
    const s = await reviewed();
    const c = s.a.confirm("job-1");
    s.confirms[0]!.d.reject(new ControlError(429, "restore_busy", "busy"));
    await c;
    expect(s.a.holdsConfirmation("job-1")).toBe(true);
    expect(s.a.current.confirmable).toBe("job-1");
    expect(s.a.current.confirm).toMatchObject({ kind: "refused", error: { kind: "capacity" } });
    expect(s.confirms).toHaveLength(1); // not retried
    await s.a.confirm("job-1"); // the review is closed: nothing sent
    expect(s.confirms).toHaveLength(1);
    s.a.review("job-1");
    s.a.acknowledge("job-1", true);
    void s.a.confirm("job-1");
    expect(s.confirms).toHaveLength(2);
  });

  it("definitive refusals discard the capability; mismatch differs from lost access", async () => {
    const run = async (e: ControlError) => {
      const s = await reviewed();
      const c = s.a.confirm("job-1");
      s.confirms[0]!.d.reject(e);
      await c;
      return s;
    };
    for (const [e, title] of [
      [new ControlError(403, "confirmation_mismatch", "m"), "Confirmation refused"],
      [new ControlError(409, "already_confirmed", "m"), "Already confirmed"],
      [new ControlError(410, "expired", "This prepared restore expired."), "No longer available"],
      [new ControlError(404, "not_found", "m"), "Restore job not found"],
      [new ControlError(503, "shutting_down", "m"), "Server shutting down"],
      [new ControlError(500, "internal", "The restore could not be started."), "Restore could not be started"],
    ] as const) {
      const s = await run(e);
      expect(s.a.holdsConfirmation("job-1"), e.code).toBe(false);
      expect(s.a.current.confirm, e.code).toMatchObject({ kind: "refused", error: { title } });
      expect(s.refusals, e.code).toEqual([]); // not an access loss
      expect(s.rechecks(), e.code).toBe(1); // the status is re-read
    }
    for (const [e, kind] of [
      [new ControlError(401, "unauthorized", "m"), "session"],
      [new ControlError(403, "loopback_required", "m"), "loopback"],
      [new ControlError(403, "forbidden", "m"), "access"],
    ] as const) {
      const s = await run(e);
      expect(s.a.holdsConfirmation("job-1"), e.code).toBe(false);
      expect(s.refusals, e.code).toEqual([kind]); // privileged data dropped at once
    }
  });

  it("a lost answer is uncertain: never resent; settled only by a list fetched afterwards", async () => {
    const s = await reviewed();
    const c = s.a.confirm("job-1");
    s.confirms[0]!.d.reject(new ControlError(0, "confirm_interrupted", "lost"));
    await c;
    expect(s.a.current.confirm).toEqual({ kind: "uncertain", jobId: "job-1" });
    expect(s.rechecks()).toBe(1);
    s.a.review("job-1"); // no second attempt while unresolved
    await s.a.confirm("job-1");
    void s.a.cancel("job-1");
    expect([s.confirms.length, s.cancels.length]).toEqual([1, 0]);
    // A list from before the answer (version 1 = the recheck's outdated version) settles nothing.
    s.a.reconcile([MAPPED], null, 1);
    expect(s.a.current.confirm.kind).toBe("uncertain");
    // A fresh list showing it still ready: not started; a new explicit decision is required.
    s.a.reconcile([MAPPED], null, 2);
    expect(s.a.current.confirm).toEqual({ kind: "not_started", jobId: "job-1" });
    expect(s.confirms).toHaveLength(1);
    expect(s.a.current.confirmable).toBe("job-1");
    s.a.review("job-1");
    expect(s.a.current.confirm.kind).toBe("reviewing");
  });

  it("a lost answer that the server had in fact accepted: the fresh list shows it running, then its result", async () => {
    const s = await reviewed();
    const c = s.a.confirm("job-1");
    s.confirms[0]!.d.reject(new ControlError(0, "confirm_interrupted", "lost"));
    await c;
    expect(s.a.current.upload.kind).toBe("prepared");
    s.a.reconcile([job({ ...MAPPED, state: "executing", usable: false })], null, 2);
    expect(s.a.current.confirm).toEqual({ kind: "accepted", jobId: "job-1" });
    expect(s.a.holdsConfirmation("job-1")).toBe(false);
    expect(s.a.current.upload.kind).toBe("idle"); // the "nothing restored" preview is gone
    s.a.reconcile([job({ ...MAPPED, state: "succeeded", usable: false })], null, 3);
    expect(s.a.current.confirm.kind).toBe("idle"); // the row shows the result
    // Unavailable status keeps the uncertainty (never success or failure).
    const t = await reviewed();
    const d = t.a.confirm("job-1");
    t.confirms[0]!.d.reject(new ControlError(0, "confirm_interrupted", "lost"));
    await d;
    t.a.reconcile(null, "transient", 2);
    expect(t.a.current.confirm.kind).toBe("uncertain");
  });

  it("an accepted job that disappears from a fresh list is reported as gone", async () => {
    const s = await reviewed();
    const c = s.a.confirm("job-1");
    s.confirms[0]!.d.resolve({ job_id: "job-1", state: "executing" });
    await c;
    s.a.reconcile([MAPPED], null, 1); // an older list (still ready): ignored, never usable again
    expect(s.a.current.confirm.kind).toBe("accepted");
    expect(s.a.current.confirmable).toBeNull();
    s.a.reconcile([], null, 2);
    expect(s.a.current.confirm).toEqual({ kind: "gone", jobId: "job-1" });
  });

  it("unmount during a confirmation forgets everything and reports nothing", async () => {
    const s = await reviewed();
    const c = s.a.confirm("job-1");
    const seen = s.states.length;
    s.a.dispose();
    expect(s.a.holdsConfirmation("job-1")).toBe(false);
    s.confirms[0]!.d.resolve({ job_id: "job-1", state: "executing" });
    await c;
    expect(s.states).toHaveLength(seen);
    expect(s.rechecks()).toBe(0);
  });

  it("a cancellation in flight blocks the review and the confirmation", async () => {
    const s = setup();
    s.a.select(zip());
    const p = s.a.upload();
    s.uploads[0]!.d.resolve({ job: MAPPED, confirmation: SECRET });
    await p;
    const c = s.a.cancel("job-1");
    s.a.review("job-1");
    expect(s.a.current.confirm.kind).toBe("idle");
    s.cancels[0]!.d.resolve();
    await c;
    expect(s.a.current.confirmable).toBeNull();
    s.a.review("job-1");
    expect(s.a.current.confirm.kind).toBe("idle");
  });
});

describe("1G-C: older lists cannot undo an accepted restore", () => {
  it("after 202, a GET from before it is discarded; the fresh list decides", async () => {
    const w = wired();
    // Wire reconcile as the component does.
    const reconcile = () => w.a.reconcile(w.list().jobs, w.list().error?.kind ?? null, w.loader.serverVersion);
    w.a.select(zip());
    const u = w.a.upload();
    w.uploads[0]!.resolve({ job: MAPPED, confirmation: SECRET });
    await u;
    w.gets[0]!.resolve([MAPPED]);
    await settle();
    reconcile();
    const old = w.loader.refresh(); // in flight before the confirmation (stays pending)
    w.a.review("job-1");
    w.a.acknowledge("job-1", true);
    const c = w.a.confirm("job-1");
    w.confirms[0]!.resolve({ job_id: "job-1", state: "executing" });
    await c;
    expect(w.a.current.confirm.kind).toBe("accepted");
    w.gets[1]!.resolve([MAPPED]); // the old answer: still "ready"
    await old;
    await settle();
    reconcile();
    expect(w.list().jobs?.[0]?.state).toBe("ready"); // unchanged (discarded, not applied)
    expect(w.a.current.confirm.kind).toBe("accepted");
    expect(w.a.current.confirmable).toBeNull();
    w.gets[2]!.resolve([job({ ...MAPPED, state: "succeeded", usable: false })]);
    await settle();
    reconcile();
    expect(w.list().jobs?.[0]?.state).toBe("succeeded");
    // A still older answer can no longer revert it.
    expect(w.gets).toHaveLength(3);
  });
});

// ------------------------------------------------------------------ 1G-C corrective pass

/**
 * The section's wiring exactly as RestoreJobs.tsx does it, including the effect that calls
 * reconcile(jobs, error, serverVersion) on every list state change.
 */
function live() {
  const gets: ReturnType<typeof deferred<RestoreJob[]>>[] = [];
  const uploads: ReturnType<typeof deferred<PreparedRestore>>[] = [];
  const confirms: { body: RestoreConfirmation; d: ReturnType<typeof deferred<RestoreAccepted>> }[] = [];
  let list: RestoreJobsState = { jobs: null, loading: false, error: null };
  let actions: RestoreActions | null = null;
  const loader = new RestoreJobsLoader(
    () => {
      const d = deferred<RestoreJob[]>();
      gets.push(d);
      return d.promise;
    },
    (s) => {
      list = s;
      actions?.reconcile(s.jobs, s.error?.kind ?? null, loader.serverVersion);
    },
  );
  const a = new RestoreActions(
    {
      uploadRestore: () => {
        const d = deferred<PreparedRestore>();
        uploads.push(d);
        return d.promise;
      },
      cancelRestore: () => Promise.reject(new Error("unused")),
      confirmRestore: (_id, body) => {
        const d = deferred<RestoreAccepted>();
        confirms.push({ body, d });
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
      recheck: () => {
        const outdated = loader.invalidate();
        void loader.refreshAfter();
        return outdated;
      },
    },
  );
  actions = a;
  return { loader, a, gets, uploads, confirms, list: () => list };
}

/** Upload MAPPED, let an authoritative GET show it ready, then review and acknowledge. */
async function readyToConfirm() {
  const w = live();
  w.a.select(zip());
  const u = w.a.upload();
  w.uploads[0]!.resolve({ job: MAPPED, confirmation: SECRET });
  await u;
  await settle();
  expect(w.gets).toHaveLength(1); // the post-upload refresh
  w.gets[0]!.resolve([MAPPED]); // authoritative: job ready
  await settle();
  expect(w.list()).toMatchObject({ jobs: [{ job_id: "job-1", state: "ready" }], error: null });
  w.a.review("job-1");
  w.a.acknowledge("job-1", true);
  return w;
}

/** Submit the confirmation and settle it with `outcome`. */
async function submit(w: ReturnType<typeof live>, outcome: "lost" | "accepted") {
  const c = w.a.confirm("job-1");
  if (outcome === "lost") w.confirms[0]!.d.reject(new ControlError(0, "confirm_interrupted", "lost"));
  else w.confirms[0]!.d.resolve({ job_id: "job-1", state: "executing" });
  await c;
  await settle();
}

const running = () => job({ ...MAPPED, state: "executing", usable: false });

describe("1G-C corrective: only a successful server answer settles a confirmation", () => {
  it("unanswered confirmation + failed status GET: stays uncertain; the old ready row is no evidence", async () => {
    const w = await readyToConfirm();
    const before = w.loader.serverVersion;
    await submit(w, "lost");
    expect(w.a.current.confirm).toEqual({ kind: "uncertain", jobId: "job-1" });
    expect(w.gets).toHaveLength(2); // the status re-read
    w.gets[1]!.reject(new ControlError(0, "unreachable", "Ignite Server is not reachable."));
    await settle();
    expect(w.list().jobs?.[0]?.state).toBe("ready"); // still shown, marked with the error
    expect(w.list().error?.kind).toBe("transient");
    expect(w.loader.serverVersion).toBe(before); // a failure is no server evidence
    expect(w.a.current.confirm).toEqual({ kind: "uncertain", jobId: "job-1" }); // not "not_started"
    expect(w.confirms).toHaveLength(1); // never resent
    // A later successful answer settles it: the server was running it.
    void w.loader.refresh();
    w.gets[2]!.resolve([running()]);
    await settle();
    expect(w.a.current.confirm).toEqual({ kind: "accepted", jobId: "job-1" });
    expect(w.confirms).toHaveLength(1);
  });

  it("unanswered confirmation + failed GET, then a successful ready answer: not started, new review allowed", async () => {
    const w = await readyToConfirm();
    await submit(w, "lost");
    w.gets[1]!.reject(new ControlError(0, "unreachable", "down"));
    await settle();
    expect(w.a.current.confirm.kind).toBe("uncertain");
    void w.loader.refresh();
    w.gets[2]!.resolve([MAPPED]);
    await settle();
    expect(w.a.current.confirm).toEqual({ kind: "not_started", jobId: "job-1" });
    expect(w.a.current.confirmable).toBe("job-1");
    expect(w.confirms).toHaveLength(1); // nothing resent by itself
    w.a.review("job-1");
    expect(w.a.current.confirm).toEqual({ kind: "reviewing", jobId: "job-1", acknowledged: false }); // explicit again
  });

  it("unanswered confirmation: terminal and missing jobs in a successful answer settle it", async () => {
    let w = await readyToConfirm();
    await submit(w, "lost");
    w.gets[1]!.resolve([job({ ...MAPPED, state: "succeeded", usable: false })]);
    await settle();
    expect(w.a.current.confirm.kind).toBe("idle"); // the row shows the server's result
    expect(w.a.holdsConfirmation("job-1")).toBe(false);
    w = await readyToConfirm();
    await submit(w, "lost");
    w.gets[1]!.resolve([]);
    await settle();
    expect(w.a.current.confirm).toEqual({ kind: "gone", jobId: "job-1" });
  });

  it("a local upsert during an unresolved confirmation settles nothing", async () => {
    const w = await readyToConfirm();
    await submit(w, "lost");
    const v = w.loader.serverVersion;
    w.loader.upsert(job({ job_id: "another" })); // e.g. a different upload's 201
    await settle();
    expect(w.loader.serverVersion).toBe(v);
    expect(w.a.current.confirm).toEqual({ kind: "uncertain", jobId: "job-1" });
    // The access-refusal path is local too: no server evidence either.
    w.loader.refuse({ kind: "transient", message: "x" });
    expect(w.loader.serverVersion).toBe(v);
  });

  it("an older answer requested before the confirmation is ignored", async () => {
    const w = await readyToConfirm();
    const older = w.loader.refresh(); // in flight before the confirmation
    await submit(w, "lost");
    w.gets[1]!.resolve([MAPPED]); // the older one answers "ready" after the confirmation
    await older;
    await settle();
    expect(w.a.current.confirm.kind).toBe("uncertain");
  });

  it("accepted 202 + failed GET: stays accepted; a later successful terminal answer shows the result", async () => {
    const w = await readyToConfirm();
    await submit(w, "accepted");
    expect(w.a.current.confirm).toEqual({ kind: "accepted", jobId: "job-1" });
    w.gets[1]!.reject(new ControlError(0, "unreachable", "down"));
    await settle();
    expect(w.a.current.confirm).toEqual({ kind: "accepted", jobId: "job-1" }); // not undone
    expect(w.a.current.confirmable).toBeNull(); // the old ready row can't make it usable again
    void w.loader.refresh();
    w.gets[2]!.resolve([running()]);
    await settle();
    expect(w.a.current.confirm).toEqual({ kind: "accepted", jobId: "job-1" });
    void w.loader.refresh();
    w.gets[3]!.resolve([job({ ...MAPPED, state: "failed", usable: false })]);
    await settle();
    expect(w.a.current.confirm.kind).toBe("idle"); // the row shows the failure
    expect(w.confirms).toHaveLength(1);
  });
});
