// Full-u64 sequence numbers in the browser (S3B). Never JS `number`: authoritative values are
// `bigint` in memory and SEQ20 (20-digit zero-padded decimal, order-preserving, the server's
// ledger encoding) in IndexedDB. No float conversion anywhere.

export const U64_MAX = 18446744073709551615n;

export class SequenceError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SequenceError";
  }
}

/** bigint → SEQ20 (e.g. 1n → "00000000000000000001"). */
export function toSeq20(v: bigint): string {
  if (v < 0n || v > U64_MAX) throw new SequenceError(`sequence ${v} outside u64`);
  return v.toString().padStart(20, "0");
}

/** SEQ20 (or plain decimal) → bigint, strictly. */
export function fromSeq20(s: string): bigint {
  if (!/^\d{1,20}$/.test(s)) throw new SequenceError(`bad sequence text ${JSON.stringify(s)}`);
  const v = BigInt(s);
  if (v > U64_MAX) throw new SequenceError(`sequence ${s} exceeds u64`);
  return v;
}

/** Next client_seq after `v`, or `null` when the replica is exhausted (never wraps). */
export function nextSeq(v: bigint): bigint | null {
  return v >= U64_MAX ? null : v + 1n;
}

export const REPLICA_SEQUENCE_EXHAUSTED = "REPLICA_SEQUENCE_EXHAUSTED";
