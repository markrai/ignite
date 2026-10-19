import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { ControlError, createControlApi, defaultBrowserName, exportDownloadPath, parseBackupExport, type FetchLike } from "../src/api/controlApi";

function recorder(responses: Record<string, { status: number; body: string }>) {
  const seen: { url: string; init: RequestInit }[] = [];
  const fetchFn: FetchLike = async (url, init) => {
    seen.push({ url, init });
    const r = responses[url] ?? { status: 404, body: "{}" };
    return new Response(r.body, { status: r.status });
  };
  return { seen, fetchFn };
}

const STATUS = JSON.stringify({
  v: 1,
  mode: "product",
  server_id: "01a0de1f-f9d6-7036-a2b7-478dc58007ff",
  server_name: "Ignite Server on Workshop",
  version: "0.1.0",
  claimed: true,
  session: null,
});

describe("D1 control API client", () => {
  it("detects product mode, and developer mode when there is no control plane", async () => {
    const product = createControlApi(recorder({ "/control/v1/status": { status: 200, body: STATUS } }).fetchFn);
    expect((await product.status())?.server_name).toBe("Ignite Server on Workshop");
    const dev404 = createControlApi(recorder({}).fetchFn);
    expect(await dev404.status()).toBeNull();
    const devStatic = createControlApi(recorder({ "/control/v1/status": { status: 200, body: "<!doctype html>" } }).fetchFn);
    expect(await devStatic.status()).toBeNull();
  });

  it("mutations are same-origin /control/v1 POSTs with the CSRF marker and never a token", async () => {
    const r = recorder({
      "/control/v1/claim": { status: 200, body: '{"device_id":"d","role":"admin","display_name":"x"}' },
      "/control/v1/pairing": { status: 201, body: '{"request_id":"r","request_secret":"s","comparison_code":"482 731","expires_in_s":300}' },
      "/control/v1/pairing/r/poll": { status: 200, body: '{"state":"pending","server_id":"x"}' },
    });
    const api = createControlApi(r.fetchFn);
    await api.claim("K7M4-P2Q9", "Firefox on Windows");
    const created = await api.requestAccess("Browser on Bobby");
    expect(created.comparison_code).toBe("482 731");
    expect((await api.poll("r", "s")).state).toBe("pending");
    for (const s of r.seen) {
      expect(s.url.startsWith("/control/v1/")).toBe(true);
      expect(s.init.method).toBe("POST");
      expect(s.init.credentials).toBe("same-origin");
      const h = s.init.headers as Record<string, string>;
      expect(h["X-Scribe-Request"]).toBe("1");
      expect(h.Authorization).toBeUndefined();
    }
    expect(JSON.parse(r.seen[1]!.init.body as string)).toEqual({ client_type: "browser", display_name: "Browser on Bobby" });
  });

  it("surfaces server refusals as ControlError (message, not secrets)", async () => {
    const api = createControlApi(recorder({ "/control/v1/claim": { status: 403, body: '{"error":"wrong_setup_code","message":"That setup code is not valid."}' } }).fetchFn);
    const e = await api.claim("0000-0000", "x").catch((x: unknown) => x);
    expect(e).toBeInstanceOf(ControlError);
    expect((e as ControlError).status).toBe(403);
    expect((e as ControlError).message).toBe("That setup code is not valid.");
  });

  it("friendly default browser names", () => {
    expect(defaultBrowserName("Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0")).toBe("Firefox on Windows");
    expect(defaultBrowserName("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15")).toBe("Safari on macOS");
    expect(defaultBrowserName("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0 Safari/537.36")).toBe("Chrome on Linux");
  });

  it("the control client never touches notebook APIs, tokens or browser storage", () => {
    const src = readFileSync(join(import.meta.dirname, "../src/api/controlApi.ts"), "utf8");
    expect(src).not.toMatch(/\/v1\/libraries|\/v1\/blobs|tokenStore|sessionStorage|localStorage|indexedDB|Authorization/);
    for (const f of [
      "components/ProductScreens.tsx",
      "components/ServerSettings.tsx",
      "components/RestoreJobs.tsx",
      "components/restoreJobsLoader.ts",
      "components/BackupExports.tsx",
      "components/backupExportsController.ts",
    ]) {
      const t = readFileSync(join(import.meta.dirname, "../src", f), "utf8");
      expect(t, f).not.toMatch(/\bfetch\s*\(|tokenStore|sessionStorage|localStorage|indexedDB/);
    }
  });
});

// ------------------------------------------------------------------ Slice 1G-A: restore jobs

const JOB = {
  v: 1,
  job_id: "01a0de1f-0000-7000-8000-00000000000a",
  state: "ready",
  usable: true,
  cleanup_pending: false,
  result: null,
  expires_in_s: 900,
  archive: { sha256: "ab".repeat(32), bytes: 123456, format: "ignite-portable-backup", format_version: 1, snapshot_version: 1 },
  scope: "current_state_only",
  not_restored: ["operation history (each restored library starts at server sequence 0)"],
  replaces_existing: false,
  existing_libraries: "never replaced, merged or modified; every library is restored under a new identity",
  totals: { libraries: 1, notebooks: 2, pages: 5, blobs: 1, blob_bytes: 1200 },
  libraries: [
    {
      source_library_id: "01a0de1f-0000-7000-8000-000000000001",
      destination_library_id: "01a0de1f-0000-7000-8000-000000000002",
      source_snapshot_sha256: "cd".repeat(32),
      source_snapshot_bytes: 4096,
      source_through_server_seq: 7,
      notebook_count: 2,
      page_count: 5,
      blob_count: 1,
    },
  ],
  warnings: [],
};

describe("1G-A restore jobs client (read-only)", () => {
  it("lists and reads jobs with same-origin, cookie-only, no-store GETs", async () => {
    const r = recorder({
      "/control/v1/admin/restore/jobs": { status: 200, body: JSON.stringify({ v: 1, jobs: [JOB] }) },
      [`/control/v1/admin/restore/jobs/${JOB.job_id}`]: { status: 200, body: JSON.stringify(JOB) },
    });
    const api = createControlApi(r.fetchFn);
    const jobs = await api.restoreJobs();
    expect(jobs).toHaveLength(1);
    expect(jobs[0]!.libraries[0]!.destination_library_id).toBe("01a0de1f-0000-7000-8000-000000000002");
    expect(jobs[0]!.result).toBeNull();
    expect((await api.restoreJob(JOB.job_id)).archive.bytes).toBe(123456);
    expect(r.seen.map((s) => s.url)).toEqual(["/control/v1/admin/restore/jobs", `/control/v1/admin/restore/jobs/${JOB.job_id}`]);
    for (const s of r.seen) {
      expect(s.init.method).toBe("GET");
      expect(s.init.credentials).toBe("same-origin");
      expect(s.init.cache).toBe("no-store");
      expect(s.init.redirect).toBe("error");
      expect(s.init.body).toBeUndefined();
      const h = s.init.headers as Record<string, string>;
      expect(h.Authorization).toBeUndefined();
      expect(h["X-Scribe-Request"]).toBeUndefined();
    }
  });

  it("parses terminal results with their publication outcome", async () => {
    const succeeded = {
      ...JOB,
      state: "succeeded",
      usable: false,
      cleanup_pending: true,
      result: {
        outcome: "succeeded",
        publication: "complete",
        report: {
          archive_sha256: JOB.archive.sha256,
          archive_bytes: 123456,
          libraries: [{ source_library_id: "s", library_id: "d", source_snapshot_sha256: "x", restored_snapshot_sha256: "y", notebook_count: 2, page_count: 5 }],
          blobs_installed: 1,
          blobs_already_present: 0,
        },
      },
    };
    const unknown = {
      ...JOB,
      state: "failed",
      usable: false,
      result: { outcome: "failed", error: "store_commit_uncertain", message: "m", publication: "unknown", unindexed_blob_files_possible: true },
    };
    const api = createControlApi(recorder({ "/control/v1/admin/restore/jobs": { status: 200, body: JSON.stringify({ v: 1, jobs: [succeeded, unknown] }) } }).fetchFn);
    const [a, b] = await api.restoreJobs();
    expect(a!.result).toMatchObject({ outcome: "succeeded", publication: "complete" });
    expect(a!.cleanup_pending).toBe(true);
    expect(b!.result).toMatchObject({ outcome: "failed", publication: "unknown", unindexed_blob_files_possible: true });
  });

  it("refusals, unknown and expired jobs are errors with the server's code", async () => {
    const api = (status: number, body: string) =>
      createControlApi(
        recorder({
          "/control/v1/admin/restore/jobs": { status, body },
          "/control/v1/admin/restore/jobs/x": { status, body },
        }).fetchFn,
      );
    const fail = async (p: Promise<unknown>) => (await p.catch((e: unknown) => e)) as ControlError;
    let e = await fail(api(401, '{"error":"unauthorized","message":"not signed in"}').restoreJobs());
    expect([e.status, e.code]).toEqual([401, "unauthorized"]);
    e = await fail(api(403, '{"error":"loopback_required","message":"only on the server"}').restoreJobs());
    expect([e.status, e.code]).toEqual([403, "loopback_required"]);
    e = await fail(api(404, '{"error":"not_found","message":"No such restore job."}').restoreJob("x"));
    expect([e.status, e.code]).toEqual([404, "not_found"]);
    // 410 is an answer for pairing polls, but an error for restore jobs.
    e = await fail(api(410, '{"error":"expired","message":"This prepared restore expired."}').restoreJob("x"));
    expect(e).toBeInstanceOf(ControlError);
    expect([e.status, e.code]).toEqual([410, "expired"]);
    const down = createControlApi(async () => {
      throw new TypeError("network");
    });
    e = await fail(down.restoreJobs());
    expect([e.status, e.code]).toEqual([0, "unreachable"]);
  });

  it("malformed or unsupported responses are refused, and unknown fields never pass through", async () => {
    const list = (body: unknown) => createControlApi(recorder({ "/control/v1/admin/restore/jobs": { status: 200, body: JSON.stringify(body) } }).fetchFn).restoreJobs();
    const code = async (p: Promise<unknown>) => ((await p.catch((e: unknown) => e)) as ControlError).code;
    expect(await code(list({ v: 2, jobs: [] }))).toBe("unsupported_response");
    expect(await code(list({ v: 1 }))).toBe("unsupported_response");
    expect(await code(list({ v: 1, jobs: [{ ...JOB, v: 2 }] }))).toBe("unsupported_response");
    expect(await code(list({ v: 1, jobs: [{ ...JOB, state: "teleported" }] }))).toBe("unsupported_response");
    expect(await code(list({ v: 1, jobs: [{ ...JOB, archive: { ...JOB.archive, bytes: "big" } }] }))).toBe("unsupported_response");
    expect(await code(list({ v: 1, jobs: [{ ...JOB, result: { outcome: "failed", publication: "maybe" } }] }))).toBe("unsupported_response");
    const html = createControlApi(recorder({ "/control/v1/admin/restore/jobs": { status: 200, body: "<!doctype html>" } }).fetchFn);
    expect(await code(html.restoreJobs())).toBe("unsupported_response");
    // A confirmation secret (never sent by the server for lists) would not survive parsing.
    const [j] = await list({ v: 1, jobs: [{ ...JOB, confirmation: "SECRET-SHOULD-NOT-APPEAR" }] });
    expect(JSON.stringify(j)).not.toContain("SECRET");
  });
});

// ------------------------------------------------------------------ Slice 1G-B: upload and cancel

const CONFIRMATION = "Q".repeat(43);

describe("1G-B restore upload and cancel client", () => {
  it("streams the original File as a raw application/zip body with the CSRF marker and cookies only", async () => {
    const r = recorder({ "/control/v1/admin/restore/jobs": { status: 201, body: JSON.stringify({ ...JOB, confirmation: CONFIRMATION }) } });
    const file = new File([new Uint8Array([0x50, 0x4b, 3, 4, 1, 2, 3])], "ignite-backup.zip", { type: "application/zip" });
    const prepared = await createControlApi(r.fetchFn).uploadRestore(file);
    const s = r.seen[0]!;
    expect(s.url).toBe("/control/v1/admin/restore/jobs");
    expect(s.init.method).toBe("POST");
    expect(s.init.body).toBe(file); // the File itself: not read, copied, wrapped or encoded
    expect(s.init.body).not.toBeInstanceOf(FormData);
    expect(typeof s.init.body).not.toBe("string");
    const h = s.init.headers as Record<string, string>;
    expect(h["Content-Type"]).toBe("application/zip");
    expect(h["X-Scribe-Request"]).toBe("1");
    expect(h.Authorization).toBeUndefined();
    expect(h["Content-Length"]).toBeUndefined(); // left to the browser
    expect([s.init.credentials, s.init.cache, s.init.redirect, s.init.referrerPolicy]).toEqual(["same-origin", "no-store", "error", "no-referrer"]);
    // The capability is separated from the ordinary summary.
    expect(prepared.confirmation).toBe(CONFIRMATION);
    expect(prepared.job.job_id).toBe(JOB.job_id);
    expect(JSON.stringify(prepared.job)).not.toContain(CONFIRMATION);
  });

  it("a 201 without a well-formed confirmation, or not a 201, is refused", async () => {
    const up = (status: number, body: unknown) =>
      createControlApi(recorder({ "/control/v1/admin/restore/jobs": { status, body: JSON.stringify(body) } }).fetchFn)
        .uploadRestore(new File(["x"], "b.zip"))
        .catch((e: unknown) => e as ControlError);
    for (const bad of [undefined, "", "short", "Q".repeat(44), "Q".repeat(42) + "=", 42]) {
      const e = (await up(201, { ...JOB, confirmation: bad })) as ControlError;
      expect(e.code, String(bad)).toBe("unsupported_response");
    }
    expect(((await up(201, { ...JOB, v: 2, confirmation: CONFIRMATION })) as ControlError).code).toBe("unsupported_response");
    expect(((await up(200, { ...JOB, confirmation: CONFIRMATION })) as ControlError).code).toBe("unsupported_response");
  });

  it("refusals keep the server's code (and archive_error); a broken connection is upload_interrupted", async () => {
    const up = async (status: number, body: unknown) =>
      (await createControlApi(recorder({ "/control/v1/admin/restore/jobs": { status, body: JSON.stringify(body) } }).fetchFn)
        .uploadRestore(new File(["x"], "b.zip"))
        .catch((e: unknown) => e)) as ControlError;
    let e = await up(413, { error: "body_too_large", message: "limit 2147483648 bytes" });
    expect([e.status, e.code]).toEqual([413, "body_too_large"]);
    e = await up(422, { error: "archive_invalid", archive_error: "zip_not_zip", message: "not a ZIP archive" });
    expect([e.status, e.code, e.detail, e.message]).toEqual([422, "archive_invalid", "zip_not_zip", "not a ZIP archive"]);
    e = await up(429, { error: "too_many_jobs", message: "Too many prepared restores." });
    expect([e.status, e.code]).toEqual([429, "too_many_jobs"]);
    e = await up(507, { error: "insufficient_storage", message: "not enough space" });
    expect([e.status, e.code]).toEqual([507, "insufficient_storage"]);
    const broken = createControlApi(async () => {
      throw new TypeError("network");
    });
    e = (await broken.uploadRestore(new File(["x"], "b.zip")).catch((x: unknown) => x)) as ControlError;
    expect([e.status, e.code]).toEqual([0, "upload_interrupted"]);
    const stop = new AbortController();
    stop.abort();
    const aborted = createControlApi(async (_u, init) => {
      if (init.signal?.aborted) throw new DOMException("aborted", "AbortError");
      throw new Error("unreachable");
    });
    e = (await aborted.uploadRestore(new File(["x"], "b.zip"), stop.signal).catch((x: unknown) => x)) as ControlError;
    expect(e.code).toBe("aborted");
  });

  it("cancel is a DELETE with the CSRF marker and no body; refusals keep their codes", async () => {
    const id = JOB.job_id;
    const r = recorder({ [`/control/v1/admin/restore/jobs/${id}`]: { status: 200, body: JSON.stringify({ ok: true, job_id: id }) } });
    await createControlApi(r.fetchFn).cancelRestore(id);
    const s = r.seen[0]!;
    expect([s.url, s.init.method, s.init.body]).toEqual([`/control/v1/admin/restore/jobs/${id}`, "DELETE", undefined]);
    const h = s.init.headers as Record<string, string>;
    expect(h["X-Scribe-Request"]).toBe("1");
    expect(h["Content-Type"]).toBeUndefined();
    expect(h.Authorization).toBeUndefined();
    expect(s.init.credentials).toBe("same-origin");
    const fail = async (status: number, body: unknown) =>
      (await createControlApi(recorder({ [`/control/v1/admin/restore/jobs/${id}`]: { status, body: JSON.stringify(body) } }).fetchFn)
        .cancelRestore(id)
        .catch((e: unknown) => e)) as ControlError;
    expect((await fail(410, { error: "expired", message: "expired" })).status).toBe(410); // an error, not an answer
    expect((await fail(409, { error: "job_executing", message: "running" })).code).toBe("job_executing");
    expect((await fail(500, { error: "cleanup_failed", message: "retrying" })).code).toBe("cleanup_failed");
  });

  it("ordinary control calls are unchanged: POSTs stay JSON, GETs carry no CSRF marker", async () => {
    const r = recorder({
      "/control/v1/admin/devices": { status: 200, body: '{"devices":[]}' },
      "/control/v1/admin/devices/d/revoke": { status: 200, body: '{"ok":true}' },
    });
    const api = createControlApi(r.fetchFn);
    await api.devices();
    await api.revoke("d");
    const [get, post] = r.seen;
    expect((get!.init.headers as Record<string, string>)["X-Scribe-Request"]).toBeUndefined();
    expect(get!.init.body).toBeUndefined();
    expect((post!.init.headers as Record<string, string>)["Content-Type"]).toBe("application/json");
    expect(post!.init.body).toBe("{}");
  });
});

// ------------------------------------------------------------------ Slice 1G-C: confirmation

describe("1G-C restore confirmation client", () => {
  const id = JOB.job_id;
  const BODY = {
    v: 1 as const,
    confirmation: CONFIRMATION,
    archive_sha256: JOB.archive.sha256,
    destinations: [{ source_library_id: "01a0de1f-0000-7000-8000-000000000001", destination_library_id: "01a0de1f-0000-7000-8000-000000000002" }],
  };
  const path = `/control/v1/admin/restore/jobs/${id}/confirm`;

  it("POSTs exactly the given body as JSON with the CSRF marker and cookies only; only a 202 for this job is accepted", async () => {
    const r = recorder({ [path]: { status: 202, body: JSON.stringify({ v: 1, job_id: id, state: "executing" }) } });
    expect(await createControlApi(r.fetchFn).confirmRestore(id, BODY)).toEqual({ job_id: id, state: "executing" });
    const s = r.seen[0]!;
    expect([s.url, s.init.method]).toEqual([path, "POST"]);
    expect(JSON.parse(s.init.body as string)).toEqual(BODY);
    const h = s.init.headers as Record<string, string>;
    expect(h["Content-Type"]).toBe("application/json");
    expect(h["X-Scribe-Request"]).toBe("1");
    expect(h.Authorization).toBeUndefined();
    expect([s.init.credentials, s.init.cache, s.init.redirect, s.init.referrerPolicy]).toEqual(["same-origin", "no-store", "error", "no-referrer"]);
  });

  it("the job id is path-encoded", async () => {
    const r = recorder({});
    await createControlApi(r.fetchFn).confirmRestore("a/b?c", BODY).catch(() => undefined);
    expect(r.seen[0]!.url).toBe("/control/v1/admin/restore/jobs/a%2Fb%3Fc/confirm");
  });

  it("any other success answer is not an acceptance", async () => {
    const answer = async (status: number, body: unknown) =>
      ((await createControlApi(recorder({ [path]: { status, body: JSON.stringify(body) } }).fetchFn)
        .confirmRestore(id, BODY)
        .catch((e: unknown) => e)) as ControlError).code;
    expect(await answer(200, { v: 1, job_id: id, state: "executing" })).toBe("unsupported_response");
    expect(await answer(201, { v: 1, job_id: id, state: "executing" })).toBe("unsupported_response");
    expect(await answer(202, { v: 2, job_id: id, state: "executing" })).toBe("unsupported_response");
    expect(await answer(202, { v: 1, job_id: "other", state: "executing" })).toBe("unsupported_response");
    expect(await answer(202, { v: 1, job_id: id, state: "succeeded" })).toBe("unsupported_response");
  });

  it("refusals keep their codes; no answer is confirm_interrupted", async () => {
    const fail = async (status: number, error: string) =>
      (await createControlApi(recorder({ [path]: { status, body: JSON.stringify({ error, message: "m" }) } }).fetchFn)
        .confirmRestore(id, BODY)
        .catch((e: unknown) => e)) as ControlError;
    for (const [status, code] of [
      [403, "confirmation_mismatch"],
      [403, "loopback_required"],
      [409, "already_confirmed"],
      [410, "expired"],
      [429, "restore_busy"],
      [503, "shutting_down"],
      [500, "internal"],
    ] as const) {
      const e = await fail(status, code);
      expect([e.status, e.code]).toEqual([status, code]);
    }
    const lost = createControlApi(async () => {
      throw new TypeError("network");
    });
    const e = (await lost.confirmRestore(id, BODY).catch((x: unknown) => x)) as ControlError;
    expect([e.status, e.code]).toEqual([0, "confirm_interrupted"]);
    expect(e.message).not.toContain(CONFIRMATION);
  });
});

// ------------------------------------------------------------------ Slice 1G-D2: backup exports

describe("1G-D2 backup export client", () => {
  const ID = "01a0de1f-0000-7000-8000-0000000000e1";
  const BASE_PATH = "/control/v1/admin/backup/exports";
  const base = { v: 1, export_id: ID, consistency: "per_library_snapshot", expires_in_s: null, archive: null, totals: null, error: null, download: null };
  const PREPARING = { ...base, state: "preparing" };
  const READY = {
    ...base,
    state: "ready",
    expires_in_s: 900,
    archive: { file_name: "ignite-backup.zip", bytes: 1234567, sha256: "ab".repeat(32), format: "ignite-portable-backup", format_version: 1 },
    totals: { libraries: 2, notebooks: 18, pages: 94, blobs: 3, blob_bytes: 123456 },
    download: `${BASE_PATH}/${ID}/download`,
  };
  const FAILED = { ...base, state: "failed", expires_in_s: 900, error: { code: "io_error", message: "A filesystem error stopped the export." } };
  const EXPIRED = { ...READY, state: "expired", expires_in_s: 0, download: null };
  const CLEANUP = { ...READY, state: "cleanup_pending", download: null };
  const list = (...exports: unknown[]) => JSON.stringify({ v: 1, exports });

  it("parses every state strictly and keeps only the known fields", async () => {
    const r = recorder({ [BASE_PATH]: { status: 200, body: list(PREPARING, { ...READY, extra: "x" }, FAILED, EXPIRED, CLEANUP) } });
    const got = await createControlApi(r.fetchFn).listBackupExports();
    expect(got.map((e) => e.state)).toEqual(["preparing", "ready", "failed", "expired", "cleanup_pending"]);
    expect(got[0]).toEqual({ export_id: ID, state: "preparing", expires_in_s: null, consistency: "per_library_snapshot", archive: null, totals: null, error: null, download: null });
    const readyFields: Record<string, unknown> = { ...READY };
    delete readyFields.v;
    expect(got[1]).toEqual(readyFields);
    expect(Object.keys(got[1]!)).not.toContain("extra");
    expect(got[2]!.error).toEqual({ code: "io_error", message: "A filesystem error stopped the export." });
    expect(parseBackupExport(READY).download).toBe(exportDownloadPath(ID));
    for (const s of [r.seen[0]!]) {
      expect(s.init.method).toBe("GET");
      expect(s.init).toMatchObject({ cache: "no-store", credentials: "same-origin", redirect: "error", referrerPolicy: "no-referrer" });
      const h = s.init.headers as Record<string, string>;
      expect(h["X-Scribe-Request"]).toBeUndefined();
      expect(h.Authorization).toBeUndefined();
    }
  });

  it("refuses malformed or contradictory summaries instead of coercing them", async () => {
    const bad: [string, unknown][] = [
      ["version", { ...READY, v: 2 }],
      ["unknown state", { ...READY, state: "done" }],
      ["unknown state, nothing else wrong", { ...PREPARING, state: "archived" }],
      ["unknown state of a failed export", { ...FAILED, state: "aborted" }],
      ["id", { ...READY, export_id: "../../x", download: `${BASE_PATH}/..%2F..%2Fx/download` }],
      ["consistency", { ...READY, consistency: "atomic" }],
      ["ready without archive", { ...READY, archive: null }],
      ["ready without totals", { ...READY, totals: null }],
      ["ready without expiry", { ...READY, expires_in_s: null }],
      ["ready without download", { ...READY, download: null }],
      ["absolute download", { ...READY, download: `http://evil.example${BASE_PATH}/${ID}/download` }],
      ["protocol-relative download", { ...READY, download: `//evil.example${BASE_PATH}/${ID}/download` }],
      ["another export's download", { ...READY, download: `${BASE_PATH}/01a0de1f-0000-7000-8000-0000000000e2/download` }],
      ["query", { ...READY, download: `${BASE_PATH}/${ID}/download?next=/x` }],
      ["fragment", { ...READY, download: `${BASE_PATH}/${ID}/download#x` }],
      ["other path", { ...READY, download: `/control/v1/admin/restore/jobs/${ID}` }],
      ["download while preparing", { ...PREPARING, download: `${BASE_PATH}/${ID}/download` }],
      ["download while failed", { ...FAILED, download: `${BASE_PATH}/${ID}/download` }],
      ["preparing with archive", { ...PREPARING, archive: READY.archive }],
      ["failed without error", { ...FAILED, error: null }],
      ["bad sha", { ...READY, archive: { ...READY.archive, sha256: "AB".repeat(32) } }],
      ["negative count", { ...READY, totals: { ...READY.totals, pages: -1 } }],
      ["fractional count", { ...READY, totals: { ...READY.totals, pages: 1.5 } }],
      ["string count", { ...READY, archive: { ...READY.archive, bytes: "12" } }],
      ["missing message", { ...FAILED, error: { code: "x" } }],
      ["not an object", "ready"],
    ];
    for (const [what, v] of bad) expect(() => parseBackupExport(v), what).toThrow(ControlError);
    expect(() => parseBackupExport({ ...READY, state: "done" })).toThrow(/unexpected backup export response/);
    // One bad entry refuses the whole list (never a partial, silently filtered list).
    const r = recorder({ [BASE_PATH]: { status: 200, body: list(PREPARING, { ...READY, archive: null }) } });
    expect(((await createControlApi(r.fetchFn).listBackupExports().catch((x: unknown) => x)) as ControlError).code).toBe("unsupported_response");
    const v2 = recorder({ [BASE_PATH]: { status: 200, body: JSON.stringify({ v: 2, exports: [] }) } });
    expect(((await createControlApi(v2.fetchFn).listBackupExports().catch((x: unknown) => x)) as ControlError).code).toBe("unsupported_response");
  });

  it("start is a body-less POST with the CSRF marker and the session cookie only; 202 is preparing, not ready", async () => {
    const r = recorder({ [BASE_PATH]: { status: 202, body: JSON.stringify(PREPARING) } });
    const e = await createControlApi(r.fetchFn).startBackupExport();
    expect(e.state).toBe("preparing");
    expect(e.download).toBeNull();
    const [req] = r.seen;
    expect(req!.url).toBe(BASE_PATH);
    expect(req!.init.method).toBe("POST");
    expect(req!.init.body).toBeUndefined();
    expect(req!.init).toMatchObject({ cache: "no-store", credentials: "same-origin", redirect: "error", referrerPolicy: "no-referrer" });
    const h = req!.init.headers as Record<string, string>;
    expect(h["X-Scribe-Request"]).toBe("1");
    expect(h["Content-Type"]).toBeUndefined();
    expect(h.Authorization).toBeUndefined();
  });

  it("only an exact 202 with a preparing summary counts as accepted", async () => {
    for (const [status, body] of [
      [200, PREPARING],
      [201, PREPARING],
      [202, READY],
      [202, { v: 1 }],
    ] as const) {
      const r = recorder({ [BASE_PATH]: { status, body: JSON.stringify(body) } });
      const e = await createControlApi(r.fetchFn)
        .startBackupExport()
        .catch((x: unknown) => x);
      expect((e as ControlError).code, `${status}`).toBe("unsupported_response");
    }
  });

  it("refusals keep the server's codes; a POST without an answer is interrupted (status 0)", async () => {
    const fail = async (status: number, body: unknown) => {
      const r = recorder({ [BASE_PATH]: { status, body: JSON.stringify(body) } });
      return (await createControlApi(r.fetchFn)
        .startBackupExport()
        .catch((x: unknown) => x)) as ControlError;
    };
    for (const code of ["export_exists", "export_busy", "too_many_exports"]) expect((await fail(429, { error: code, message: "m" })).code).toBe(code);
    expect((await fail(403, { error: "loopback_required", message: "m" })).status).toBe(403);
    expect((await fail(403, { error: "browser_session_required", message: "m" })).code).toBe("browser_session_required");
    expect((await fail(507, { error: "insufficient_storage", message: "m" })).code).toBe("insufficient_storage");
    expect((await fail(503, { error: "shutting_down", message: "m" })).status).toBe(503);
    const broken = createControlApi(() => Promise.reject(new TypeError("network")));
    const e = (await broken.startBackupExport().catch((x: unknown) => x)) as ControlError;
    expect([e.status, e.code]).toEqual([0, "interrupted"]);
    const g = (await broken.listBackupExports().catch((x: unknown) => x)) as ControlError;
    expect([g.status, g.code]).toEqual([0, "unreachable"]);
  });

  it("reads and dismisses one export by its encoded id; DELETE has no body; deferred cleanup is reported", async () => {
    const r = recorder({ [`${BASE_PATH}/${ID}`]: { status: 200, body: JSON.stringify(READY) } });
    expect((await createControlApi(r.fetchFn).getBackupExport(ID)).state).toBe("ready");
    const gone = recorder({ [`${BASE_PATH}/${ID}`]: { status: 410, body: '{"error":"expired","message":"m"}' } });
    expect(((await createControlApi(gone.fetchFn).getBackupExport(ID).catch((x: unknown) => x)) as ControlError).status).toBe(410);

    const del = (body: unknown, status = 200) => recorder({ [`${BASE_PATH}/${ID}`]: { status, body: JSON.stringify(body) } });
    let d = del({ ok: true, export_id: ID });
    expect(await createControlApi(d.fetchFn).dismissBackupExport(ID)).toEqual({ export_id: ID, cleanup: "done" });
    expect(d.seen[0]!.init.method).toBe("DELETE");
    expect(d.seen[0]!.init.body).toBeUndefined();
    expect((d.seen[0]!.init.headers as Record<string, string>)["X-Scribe-Request"]).toBe("1");
    expect((d.seen[0]!.init.headers as Record<string, string>).Authorization).toBeUndefined();
    d = del({ ok: true, export_id: ID, cleanup: "deferred" });
    expect((await createControlApi(d.fetchFn).dismissBackupExport(ID)).cleanup).toBe("deferred");
    for (const [status, code] of [
      [409, "export_preparing"],
      [404, "not_found"],
      [500, "cleanup_failed"],
    ] as const) {
      d = del({ error: code, message: "m" }, status);
      expect(((await createControlApi(d.fetchFn).dismissBackupExport(ID).catch((x: unknown) => x)) as ControlError).code).toBe(code);
    }
    d = del({ ok: true, export_id: "someone-else" });
    expect(((await createControlApi(d.fetchFn).dismissBackupExport(ID).catch((x: unknown) => x)) as ControlError).code).toBe("unsupported_response");
    const odd = recorder({});
    await createControlApi(odd.fetchFn)
      .getBackupExport("a/b?c")
      .catch(() => undefined);
    expect(odd.seen[0]!.url).toBe(`${BASE_PATH}/a%2Fb%3Fc`);
  });

  it("the download path is built locally for exactly one export", () => {
    expect(exportDownloadPath(ID)).toBe(`/control/v1/admin/backup/exports/${ID}/download`);
    expect(exportDownloadPath("a/../b")).toBe("/control/v1/admin/backup/exports/a%2F..%2Fb/download");
  });
});
