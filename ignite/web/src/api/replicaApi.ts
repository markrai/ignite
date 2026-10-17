// S3B replica API: the browser's ONLY write surface, and it is used only by the sync
// coordinator (`replica/coordinator.ts`) — never by React components or pointer handlers.
//
// It can issue exactly one kind of request: POST /v1/libraries/{id}/operations with a batch of
// already-encoded canonical envelopes, sent verbatim (the body is assembled by concatenation,
// never re-serialized). No blob PUT, no genesis POST, no other method or path.
//
// The bearer token goes only in the Authorization header of this same-origin request; it never
// appears in URLs, errors or logs.

import { parseLossless } from "../model/lossless";
import { uuid as checkUuid } from "../model/validate";
import type { FetchLike } from "./readOnlyApi";

/** Server batch limit is 100; the browser sends at most this many per request. */
export const MAX_BATCH = 50;

export interface OperationAck {
  operation_id: string;
  server_seq: bigint;
  outcome: "applied" | "noop";
  duplicate: boolean;
}

export interface SubmitAck {
  head_server_seq: bigint;
  results: OperationAck[];
}

/** Network failure, 5xx, 408 or 429: retry the same bytes later. */
export class TransientError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "TransientError";
  }
}

/** 401: the token is missing, wrong or revoked. The outbox is kept. */
export class ReplicaUnauthorizedError extends Error {
  constructor() {
    super("The server rejected the connection (token missing or invalid).");
    this.name = "ReplicaUnauthorizedError";
  }
}

/** A definitive refusal (sequencing/identity 409, semantic 422, malformed 400, 404…). Fail closed. */
export class RefusedError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly envelopeIndex: number | null,
  ) {
    super(message);
    this.name = "RefusedError";
  }
}

export interface ReplicaApi {
  submit(libraryId: string, envelopes: readonly { operation_id: string; envelope: string }[]): Promise<SubmitAck>;
}

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

function validateAck(body: unknown, libraryId: string, sent: readonly { operation_id: string }[]): SubmitAck {
  const bad = (m: string): never => {
    throw new RefusedError(200, "malformed_ack", `Malformed server acknowledgement: ${m}`, null);
  };
  if (typeof body !== "object" || body === null) return bad("not an object");
  const b = body as Record<string, unknown>;
  if (b.library_id !== libraryId) bad("library id mismatch");
  if (typeof b.head_server_seq !== "bigint") bad("head_server_seq");
  if (!Array.isArray(b.results) || b.results.length !== sent.length) return bad("result count");
  const results = (b.results as unknown[]).map((r, i): OperationAck => {
    if (typeof r !== "object" || r === null) return bad("result");
    const x = r as Record<string, unknown>;
    if (x.operation_id !== sent[i]!.operation_id) bad(`result ${i} operation id`);
    if (typeof x.server_seq !== "bigint" || x.server_seq < 1n) bad(`result ${i} server_seq`);
    if (x.outcome !== "applied" && x.outcome !== "noop") bad(`result ${i} outcome`);
    if (typeof x.duplicate !== "boolean") bad(`result ${i} duplicate`);
    return x as unknown as OperationAck;
  });
  return { head_server_seq: b.head_server_seq as bigint, results };
}

export function createReplicaApi(token: string | null, fetchFn: FetchLike = (i, init) => fetch(i, init)): ReplicaApi {
  return {
    async submit(libraryId, batch) {
      const id = checkUuid(libraryId, "library id");
      if (batch.length === 0 || batch.length > MAX_BATCH) throw new RefusedError(0, "invalid_batch_size", "internal: bad batch size", null);
      // Exact bytes: each envelope is the stored canonical text, joined without re-encoding.
      const body = `{"envelopes":[${batch.map((e) => e.envelope).join(",")}]}`;
      // D1: the CSRF marker is required for cookie-authenticated mutations (harmless otherwise).
      const headers: Record<string, string> = { "Content-Type": "application/json", Accept: "application/json", "X-Scribe-Request": "1" };
      if (token) headers.Authorization = `Bearer ${token}`;
      let resp: Response;
      try {
        resp = await fetchFn(`/v1/libraries/${id}/operations`, {
          method: "POST",
          headers,
          body,
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
      let parsed: unknown;
      try {
        parsed = parseLossless(text);
      } catch {
        if (resp.ok) throw new TransientError(resp.status, "Malformed acknowledgement; will retry.");
        throw new RefusedError(resp.status, "http_error", `Server refused the operation (${resp.status}).`, null);
      }
      if (!resp.ok) {
        const e = (parsed ?? {}) as Record<string, unknown>;
        const code = str(e.semantic_code) || str(e.error) || "http_error";
        const idx = typeof e.envelope_index === "number" ? e.envelope_index : null;
        throw new RefusedError(resp.status, code, str(e.message) || `Server refused the operation (${resp.status}).`, idx);
      }
      return validateAck(parsed, id, batch);
    },
  };
}
