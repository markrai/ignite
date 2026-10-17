// S3A read-only API client. THIS MODULE IS THE WHOLE BROWSER ↔ SERVER SURFACE.
//
// It can only issue GET requests to same-origin `/v1/...` paths: libraries, one library's head,
// its current snapshot, and blobs. There is deliberately no way to POST operations or genesis,
// PUT blobs, or DELETE anything — ignite-web S3A cannot modify notebook state. (Tested; the
// Playwright gate also fails on any non-GET `/v1` request.)
//
// The bearer token is sent only in the Authorization header of these same-origin requests. It
// never appears in URLs, errors or logs.

import { parseLossless } from "../model/lossless";
import type { LibraryHead, LibrarySummary, SnapshotResponse } from "../model/types";
import {
  sha256 as checkSha,
  uuid as checkUuid,
  validateLibraryHead,
  validateLibraryList,
  validateSnapshotResponse,
} from "../model/validate";

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/** 401: the token is missing, wrong or revoked. */
export class UnauthorizedError extends ApiError {
  constructor() {
    super(401, "The server rejected the connection (token missing or invalid).");
    this.name = "UnauthorizedError";
  }
}

export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

export interface ReadOnlyApi {
  listLibraries(): Promise<LibrarySummary[]>;
  libraryHead(libraryId: string): Promise<LibraryHead>;
  snapshot(libraryId: string): Promise<SnapshotResponse>;
  blob(sha256: string): Promise<Uint8Array>;
}

/** The only request primitive: method is fixed to GET and the path to same-origin `/v1/`. */
async function get(fetchFn: FetchLike, token: string | null, path: string): Promise<Response> {
  if (!path.startsWith("/v1/") || path.includes("..") || path.includes("//")) {
    throw new ApiError(0, "internal: refused non-/v1 path");
  }
  const headers: Record<string, string> = { Accept: "application/json" };
  if (token) headers.Authorization = `Bearer ${token}`;
  let resp: Response;
  try {
    resp = await fetchFn(path, {
      method: "GET",
      headers,
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      referrerPolicy: "no-referrer",
    });
  } catch {
    throw new ApiError(0, "The sync server is unreachable.");
  }
  if (resp.status === 401) throw new UnauthorizedError();
  if (!resp.ok) throw new ApiError(resp.status, `Server error ${resp.status} for ${path.split("?")[0]}`);
  return resp;
}

async function json(resp: Response): Promise<unknown> {
  try {
    // Lossless: u64 sequence fields become bigint (JSON.parse would round them above 2^53).
    return parseLossless(await resp.text());
  } catch {
    throw new ApiError(resp.status, "Malformed JSON from server");
  }
}

export function createReadOnlyApi(token: string | null, fetchFn: FetchLike = (i, init) => fetch(i, init)): ReadOnlyApi {
  return {
    async listLibraries() {
      return validateLibraryList(await json(await get(fetchFn, token, "/v1/libraries")));
    },
    async libraryHead(libraryId) {
      const id = checkUuid(libraryId, "library id");
      const head = validateLibraryHead(await json(await get(fetchFn, token, `/v1/libraries/${id}`)));
      if (head.library_id !== id) throw new ApiError(0, "library id mismatch");
      return head;
    },
    async snapshot(libraryId) {
      const id = checkUuid(libraryId, "library id");
      return validateSnapshotResponse(await json(await get(fetchFn, token, `/v1/libraries/${id}/snapshot`)), id);
    },
    async blob(sha) {
      const s = checkSha(sha, "blob");
      const resp = await get(fetchFn, token, `/v1/blobs/${s}`);
      return new Uint8Array(await resp.arrayBuffer());
    },
  };
}
