// D1 Scribe Server control plane (`/control/v1`). Product mode only.
//
// The browser never handles a credential: the server sets an HttpOnly, SameSite=Strict session
// cookie on claim/approval, and every request here is same-origin with `credentials:
// "same-origin"`. Mutations carry `X-Scribe-Request: 1` (the server's CSRF marker; a
// cross-origin page cannot send it without a CORS preflight, which is never granted).

export type ClientType = "kindle" | "browser";
export type Role = "admin" | "member";

export interface SessionInfo {
  device_id: string;
  role: Role;
  display_name: string;
}

export interface ServerStatusPublic {
  v: number;
  mode: "product";
  server_id: string;
  server_name: string;
  version: string;
  claimed: boolean;
  session: SessionInfo | null;
}

export interface PairingCreated {
  request_id: string;
  request_secret: string;
  comparison_code: string;
  expires_in_s: number;
}

export type PairingState = "pending" | "approved" | "denied" | "expired" | "delivered";

export interface PendingRequest {
  request_id: string;
  client_type: ClientType;
  display_name: string;
  comparison_code: string;
  expires_in_s: number;
}

export interface DeviceRecord {
  device_id: string;
  client_type: ClientType;
  display_name: string;
  role: Role;
  created_ms: number;
  last_seen_ms: number | null;
  revoked: boolean;
  current: boolean;
}

export interface ServerStatusAdmin {
  server_id: string;
  server_name: string;
  version: string;
  started_ms: number;
  library_count: number;
  device_count: number;
  pending_requests: number;
}

// --- Slice 1F/1G-A: backup restore jobs (read-only in the browser) ------------------------
//
// Mirrors the summaries `scribe-server` returns for `GET /control/v1/admin/restore/jobs[/{id}]`
// (ignite/server/scribe-server/src/restore.rs). Those never contain the one-time confirmation
// secret, and `parseRestoreJob` copies known fields only, so nothing else can reach the UI.

/** `expired`: a ready job past its deadline that the server has not withdrawn yet. */
export type RestoreJobState = "ready" | "executing" | "succeeded" | "failed" | "expired" | "cleanup_pending";
/** `complete`: published; `none`: this restore published nothing; `unknown`: COMMIT outcome unknown. */
export type Publication = "complete" | "none" | "unknown";

export interface RestoreArchive {
  sha256: string;
  bytes: number;
  format: string;
  format_version: number;
  snapshot_version: number;
}

export interface RestoreLibraryPlan {
  source_library_id: string;
  /** Proposed until a restore succeeds; never an existing library. */
  destination_library_id: string;
  source_snapshot_sha256: string;
  source_snapshot_bytes: number;
  source_through_server_seq: number;
  notebook_count: number;
  page_count: number;
  blob_count: number;
}

export interface RestoredLibrary {
  source_library_id: string;
  library_id: string;
  source_snapshot_sha256: string;
  restored_snapshot_sha256: string;
  notebook_count: number;
  page_count: number;
}

export type RestoreResult =
  | {
      outcome: "succeeded";
      publication: "complete";
      report: {
        archive_sha256: string;
        archive_bytes: number;
        libraries: RestoredLibrary[];
        blobs_installed: number;
        blobs_already_present: number;
      };
    }
  | {
      outcome: "failed";
      error: string;
      message: string;
      publication: "none" | "unknown";
      unindexed_blob_files_possible: boolean;
    };

export interface RestoreJob {
  job_id: string;
  state: RestoreJobState;
  /** Ready, not expired, not withdrawn (confirmation is not available in the browser yet). */
  usable: boolean;
  /** Staged files are still being deleted (a finished restore keeps its result). */
  cleanup_pending: boolean;
  /** Ready: until the prepared job expires; finished: until its result is forgotten. */
  expires_in_s: number;
  archive: RestoreArchive;
  scope: string;
  not_restored: string[];
  totals: { libraries: number; notebooks: number; pages: number; blobs: number; blob_bytes: number };
  libraries: RestoreLibraryPlan[];
  warnings: string[];
  /** Present once the job is `succeeded` or `failed`. */
  result: RestoreResult | null;
}

const JOB_STATES: readonly string[] = ["ready", "executing", "succeeded", "failed", "expired", "cleanup_pending"];

function unsupported(what: string): ControlError {
  return new ControlError(0, "unsupported_response", `The server returned an unexpected restore response (${what}).`);
}

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);
function str(o: Obj, k: string): string {
  const v = o[k];
  if (typeof v !== "string") throw unsupported(k);
  return v;
}
function num(o: Obj, k: string): number {
  const v = o[k];
  if (typeof v !== "number" || !Number.isFinite(v) || v < 0) throw unsupported(k);
  return v;
}
function bool(o: Obj, k: string): boolean {
  const v = o[k];
  if (typeof v !== "boolean") throw unsupported(k);
  return v;
}
function obj(o: Obj, k: string): Obj {
  const v = o[k];
  if (!isObj(v)) throw unsupported(k);
  return v;
}
function list(o: Obj, k: string): unknown[] {
  const v = o[k];
  if (!Array.isArray(v)) throw unsupported(k);
  return v;
}
const strings = (o: Obj, k: string): string[] =>
  list(o, k).map((x) => {
    if (typeof x !== "string") throw unsupported(k);
    return x;
  });

function parseResult(v: unknown): RestoreResult | null {
  if (v === null || v === undefined) return null;
  if (!isObj(v)) throw unsupported("result");
  if (v.outcome === "succeeded" && v.publication === "complete") {
    const r = obj(v, "report");
    return {
      outcome: "succeeded",
      publication: "complete",
      report: {
        archive_sha256: str(r, "archive_sha256"),
        archive_bytes: num(r, "archive_bytes"),
        libraries: list(r, "libraries").map((l) => {
          if (!isObj(l)) throw unsupported("report.libraries");
          return {
            source_library_id: str(l, "source_library_id"),
            library_id: str(l, "library_id"),
            source_snapshot_sha256: str(l, "source_snapshot_sha256"),
            restored_snapshot_sha256: str(l, "restored_snapshot_sha256"),
            notebook_count: num(l, "notebook_count"),
            page_count: num(l, "page_count"),
          };
        }),
        blobs_installed: num(r, "blobs_installed"),
        blobs_already_present: num(r, "blobs_already_present"),
      },
    };
  }
  if (v.outcome === "failed" && (v.publication === "none" || v.publication === "unknown")) {
    return {
      outcome: "failed",
      error: str(v, "error"),
      message: str(v, "message"),
      publication: v.publication,
      unindexed_blob_files_possible: bool(v, "unindexed_blob_files_possible"),
    };
  }
  throw unsupported("result");
}

/** Validate one job summary (version 1), keeping only the fields above. */
export function parseRestoreJob(v: unknown): RestoreJob {
  if (!isObj(v) || v.v !== 1) throw unsupported("version");
  const state = str(v, "state");
  if (!JOB_STATES.includes(state)) throw unsupported("state");
  const a = obj(v, "archive");
  const t = obj(v, "totals");
  return {
    job_id: str(v, "job_id"),
    state: state as RestoreJobState,
    usable: bool(v, "usable"),
    cleanup_pending: bool(v, "cleanup_pending"),
    expires_in_s: num(v, "expires_in_s"),
    archive: {
      sha256: str(a, "sha256"),
      bytes: num(a, "bytes"),
      format: str(a, "format"),
      format_version: num(a, "format_version"),
      snapshot_version: num(a, "snapshot_version"),
    },
    scope: str(v, "scope"),
    not_restored: strings(v, "not_restored"),
    totals: {
      libraries: num(t, "libraries"),
      notebooks: num(t, "notebooks"),
      pages: num(t, "pages"),
      blobs: num(t, "blobs"),
      blob_bytes: num(t, "blob_bytes"),
    },
    libraries: list(v, "libraries").map((l) => {
      if (!isObj(l)) throw unsupported("libraries");
      return {
        source_library_id: str(l, "source_library_id"),
        destination_library_id: str(l, "destination_library_id"),
        source_snapshot_sha256: str(l, "source_snapshot_sha256"),
        source_snapshot_bytes: num(l, "source_snapshot_bytes"),
        source_through_server_seq: num(l, "source_through_server_seq"),
        notebook_count: num(l, "notebook_count"),
        page_count: num(l, "page_count"),
        blob_count: num(l, "blob_count"),
      };
    }),
    warnings: strings(v, "warnings"),
    result: parseResult(v.result),
  };
}

export class ControlError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    /** A finer server code when there is one (e.g. `archive_error` of a refused backup). */
    readonly detail: string | null = null,
  ) {
    super(message);
    this.name = "ControlError";
  }
}

/** The server's refusal as a ControlError (its `error`, `message` and, if any, `archive_error`). */
function refusal(status: number, json: unknown): ControlError {
  const e = (isObj(json) ? json : {}) as { error?: unknown; message?: unknown; archive_error?: unknown };
  return new ControlError(
    status,
    typeof e.error === "string" ? e.error : "http_error",
    typeof e.message === "string" ? e.message : `Server error ${status}`,
    typeof e.archive_error === "string" ? e.archive_error : null,
  );
}

/**
 * The server's compressed-upload ceiling for restore backups (`RestoreLimits::max_upload_bytes`,
 * 2 GiB). Checked locally to explain an oversized file early; the server stays authoritative.
 */
export const MAX_RESTORE_UPLOAD_BYTES = 2 * 1024 ** 3;

/**
 * A just-verified restore job plus its one-time confirmation capability. The capability exists
 * only in the 201 answer to the upload (GET never returns it) and is needed to confirm.
 */
export interface PreparedRestore {
  job: RestoreJob;
  confirmation: string;
}

/** 32 random bytes, unpadded base64url (`control_protocol::b64url`). */
const CONFIRMATION = /^[A-Za-z0-9_-]{43}$/;

/**
 * Stream `file` to the server as the raw request body (never read into memory here). A failure
 * after the request may have reached the server is `upload_interrupted`: it may still have
 * prepared the job.
 */
async function uploadRestore(fetchFn: FetchLike, file: Blob, signal?: AbortSignal): Promise<PreparedRestore> {
  let resp: Response;
  try {
    resp = await fetchFn(BASE + "/admin/restore/jobs", {
      method: "POST",
      headers: { Accept: "application/json", "Content-Type": "application/zip", "X-Scribe-Request": "1" },
      body: file,
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      referrerPolicy: "no-referrer",
      signal,
    });
  } catch {
    if (signal?.aborted) throw new ControlError(0, "aborted", "The upload was stopped.");
    throw new ControlError(
      0,
      "upload_interrupted",
      "The upload was interrupted before the server answered. The server may still have prepared this restore.",
    );
  }
  let json: unknown = null;
  try {
    json = await resp.json();
  } catch {
    // not JSON
  }
  if (!resp.ok) throw refusal(resp.status, json);
  if (resp.status !== 201 || !isObj(json)) throw unsupported("upload");
  // Taken out once; the rest is the ordinary summary, parsed by the one strict parser.
  const { confirmation, ...summary } = json;
  if (typeof confirmation !== "string" || !CONFIRMATION.test(confirmation)) throw unsupported("confirmation");
  return { job: parseRestoreJob(summary), confirmation };
}

export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

const BASE = "/control/v1";

/** `gone`: whether HTTP 410 is a normal answer (pairing polls) or an error (restore jobs). */
async function request<T>(
  fetchFn: FetchLike,
  method: "GET" | "POST" | "DELETE",
  path: string,
  body?: unknown,
  gone: "answer" | "error" = "answer",
): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };
  // Every mutation carries the CSRF marker; only POST has a (JSON) body.
  if (method !== "GET") headers["X-Scribe-Request"] = "1";
  if (method === "POST") headers["Content-Type"] = "application/json";
  let resp: Response;
  try {
    resp = await fetchFn(BASE + path, {
      method,
      headers,
      body: method === "POST" ? JSON.stringify(body ?? {}) : undefined,
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      referrerPolicy: "no-referrer",
    });
  } catch {
    throw new ControlError(0, "unreachable", "Ignite Server is not reachable.");
  }
  let json: unknown = null;
  try {
    json = await resp.json();
  } catch {
    // non-JSON (e.g. a developer sync server without a control plane)
  }
  if (!resp.ok && (resp.status !== 410 || gone === "error")) throw refusal(resp.status, json);
  return json as T;
}

export interface ControlApi {
  /** `null` when this server has no control plane (developer token mode). */
  status(): Promise<ServerStatusPublic | null>;
  claim(setupCode: string, displayName: string): Promise<SessionInfo>;
  requestAccess(displayName: string): Promise<PairingCreated>;
  poll(requestId: string, requestSecret: string): Promise<{ state: PairingState }>;
  pending(): Promise<PendingRequest[]>;
  decide(requestId: string, approve: boolean): Promise<void>;
  devices(): Promise<DeviceRecord[]>;
  revoke(deviceId: string): Promise<void>;
  server(): Promise<ServerStatusAdmin>;
  /** Clear this browser's session cookie (the device record stays until revoked). */
  logout(): Promise<void>;
  /** This administrator's restore jobs (read-only; never a confirmation secret). */
  restoreJobs(): Promise<RestoreJob[]>;
  /** One of this administrator's restore jobs (404 unknown/foreign, 410 expired/cancelled). */
  restoreJob(jobId: string): Promise<RestoreJob>;
  /** Upload a backup ZIP (the `File` itself is the body) and get its verified summary. */
  uploadRestore(file: Blob, signal?: AbortSignal): Promise<PreparedRestore>;
  /** Discard a prepared restore job (the server refuses one that is executing). */
  cancelRestore(jobId: string): Promise<void>;
}

export function createControlApi(fetchFn: FetchLike = (i, init) => fetch(i, init)): ControlApi {
  return {
    async status() {
      let resp: Response;
      try {
        resp = await fetchFn(BASE + "/status", { method: "GET", cache: "no-store", credentials: "same-origin", headers: { Accept: "application/json" } });
      } catch {
        throw new ControlError(0, "unreachable", "Ignite Server is not reachable.");
      }
      if (resp.status === 404) return null;
      let v: unknown;
      try {
        v = await resp.json();
      } catch {
        return null; // not a Scribe Server control plane
      }
      const s = v as Partial<ServerStatusPublic>;
      return resp.ok && s.mode === "product" && typeof s.server_id === "string" ? (s as ServerStatusPublic) : null;
    },
    claim: (setup_code, display_name) => request<SessionInfo>(fetchFn, "POST", "/claim", { setup_code, display_name }),
    requestAccess: (display_name) => request<PairingCreated>(fetchFn, "POST", "/pairing", { client_type: "browser", display_name }),
    poll: (id, request_secret) => request<{ state: PairingState }>(fetchFn, "POST", `/pairing/${encodeURIComponent(id)}/poll`, { request_secret }),
    pending: async () => (await request<{ requests: PendingRequest[] }>(fetchFn, "GET", "/admin/pairing")).requests,
    decide: async (id, approve) => {
      await request(fetchFn, "POST", `/admin/pairing/${encodeURIComponent(id)}/${approve ? "approve" : "deny"}`);
    },
    devices: async () => (await request<{ devices: DeviceRecord[] }>(fetchFn, "GET", "/admin/devices")).devices,
    revoke: async (id) => {
      await request(fetchFn, "POST", `/admin/devices/${encodeURIComponent(id)}/revoke`);
    },
    server: () => request<ServerStatusAdmin>(fetchFn, "GET", "/admin/server"),
    logout: async () => {
      await request(fetchFn, "POST", "/session/logout");
    },
    restoreJobs: async () => {
      const v = await request<unknown>(fetchFn, "GET", "/admin/restore/jobs", undefined, "error");
      if (!isObj(v) || v.v !== 1 || !Array.isArray(v.jobs)) throw unsupported("job list");
      return v.jobs.map(parseRestoreJob);
    },
    restoreJob: async (id) =>
      parseRestoreJob(await request<unknown>(fetchFn, "GET", `/admin/restore/jobs/${encodeURIComponent(id)}`, undefined, "error")),
    uploadRestore: (file, signal) => uploadRestore(fetchFn, file, signal),
    cancelRestore: async (id) => {
      await request(fetchFn, "DELETE", `/admin/restore/jobs/${encodeURIComponent(id)}`, undefined, "error");
    },
  };
}

/** A friendly default name for this browser ("Firefox on Windows"); the user may edit it. */
export function defaultBrowserName(ua: string = typeof navigator !== "undefined" ? navigator.userAgent : ""): string {
  const browser = /Edg\//.test(ua) ? "Edge" : /Firefox\//.test(ua) ? "Firefox" : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const os = /Windows/.test(ua) ? "Windows" : /iPhone|iPad/.test(ua) ? "iOS" : /Android/.test(ua) ? "Android" : /Mac OS X/.test(ua) ? "macOS" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${browser} on ${os}` : browser;
}
