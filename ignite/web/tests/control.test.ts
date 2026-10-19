import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { ControlError, createControlApi, defaultBrowserName, type FetchLike } from "../src/api/controlApi";

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
    for (const f of ["components/ProductScreens.tsx", "components/ServerSettings.tsx", "components/RestoreJobs.tsx", "components/restoreJobsLoader.ts"]) {
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
