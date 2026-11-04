// Content-addressed blob publication: PUT /v1/blobs/{sha256}.
//
// Used by patterned notebook creation (Slice 6B) before CreateNotebook. Same auth and CSRF
// conventions as `replicaApi`. Not an offline queue — callers must fail closed when the PUT
// cannot complete.

import { sha256 as checkSha } from "../model/validate";
import type { FetchLike } from "./readOnlyApi";
import { RefusedError, ReplicaUnauthorizedError, TransientError } from "./replicaApi";

export interface BlobPutAck {
  sha256: string;
  byte_len: number;
  created: boolean;
}

export interface BlobApi {
  /** Publish raw bytes at their SHA-256 address. Idempotent when the blob already exists. */
  put(sha256: string, bytes: Uint8Array): Promise<BlobPutAck>;
}

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

function validateAck(body: unknown, sha256: string, byteLen: number): BlobPutAck {
  if (typeof body !== "object" || body === null) {
    throw new RefusedError(200, "malformed_ack", "Malformed blob acknowledgement.", null);
  }
  const b = body as Record<string, unknown>;
  if (b.sha256 !== sha256) {
    throw new RefusedError(200, "malformed_ack", "Blob acknowledgement SHA mismatch.", null);
  }
  if (typeof b.byte_len !== "number" || b.byte_len !== byteLen) {
    throw new RefusedError(200, "malformed_ack", "Blob acknowledgement length mismatch.", null);
  }
  if (typeof b.created !== "boolean") {
    throw new RefusedError(200, "malformed_ack", "Blob acknowledgement missing created.", null);
  }
  return { sha256, byte_len: byteLen, created: b.created };
}

export function createBlobApi(token: string | null, fetchFn: FetchLike = (i, init) => fetch(i, init)): BlobApi {
  return {
    async put(sha256, bytes) {
      const sha = checkSha(sha256, "blob");
      const headers: Record<string, string> = {
        "Content-Type": "application/octet-stream",
        Accept: "application/json",
        "X-Scribe-Request": "1",
      };
      if (token) headers.Authorization = `Bearer ${token}`;
      let resp: Response;
      try {
        resp = await fetchFn(`/v1/blobs/${sha}`, {
          method: "PUT",
          headers,
          body: bytes as Uint8Array<ArrayBuffer>,
          cache: "no-store",
          credentials: "same-origin",
          redirect: "error",
          referrerPolicy: "no-referrer",
        });
      } catch {
        throw new TransientError(0, "The sync server is unreachable.");
      }
      if (resp.status === 401) throw new ReplicaUnauthorizedError();
      let text: string;
      try {
        text = await resp.text();
      } catch {
        throw new TransientError(resp.status, "The response was interrupted.");
      }
      if (resp.status >= 500 || resp.status === 408 || resp.status === 429) {
        throw new TransientError(resp.status, `Server unavailable (${resp.status}).`);
      }
      let parsed: unknown = null;
      if (text) {
        try {
          parsed = JSON.parse(text) as unknown;
        } catch {
          if (resp.ok) throw new TransientError(resp.status, "Malformed blob acknowledgement; will retry.");
          throw new RefusedError(resp.status, "http_error", `Server refused the blob (${resp.status}).`, null);
        }
      }
      if (!resp.ok) {
        const e = (parsed ?? {}) as Record<string, unknown>;
        const code = str(e.semantic_code) || str(e.error) || "http_error";
        throw new RefusedError(resp.status, code, str(e.message) || `Server refused the blob (${resp.status}).`, null);
      }
      return validateAck(parsed, sha, bytes.length);
    },
  };
}
