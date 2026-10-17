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

export class ControlError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ControlError";
  }
}

export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

const BASE = "/control/v1";

async function request<T>(fetchFn: FetchLike, method: "GET" | "POST", path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };
  if (method === "POST") {
    headers["X-Scribe-Request"] = "1";
    headers["Content-Type"] = "application/json";
  }
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
  if (!resp.ok && resp.status !== 410) {
    const e = (json ?? {}) as { error?: string; message?: string };
    throw new ControlError(resp.status, e.error ?? "http_error", e.message ?? `Server error ${resp.status}`);
  }
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
  };
}

/** A friendly default name for this browser ("Firefox on Windows"); the user may edit it. */
export function defaultBrowserName(ua: string = typeof navigator !== "undefined" ? navigator.userAgent : ""): string {
  const browser = /Edg\//.test(ua) ? "Edge" : /Firefox\//.test(ua) ? "Firefox" : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const os = /Windows/.test(ua) ? "Windows" : /iPhone|iPad/.test(ua) ? "iOS" : /Android/.test(ua) ? "Android" : /Mac OS X/.test(ua) ? "macOS" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${browser} on ${os}` : browser;
}
