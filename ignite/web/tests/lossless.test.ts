import { describe, expect, it } from "vitest";
import { createReadOnlyApi, type FetchLike } from "../src/api/readOnlyApi";
import { JsonSyntaxError, parseLossless } from "../src/model/lossless";
import { fromSeq20, nextSeq, SequenceError, toSeq20, U64_MAX } from "../src/replica/seq";
import { manifest, snapshotJson } from "./fixtures";

const BIG = ["0", "1", "9007199254740991", "9007199254740992", "9007199254740993", "9223372036854775807", "9223372036854775808", "18446744073709551615"];

describe("lossless JSON (sequence fields)", () => {
  it.each(BIG)("server_seq %s round-trips exactly as bigint", (s) => {
    const v = parseLossless(`{"results":[{"server_seq":${s}}],"head_server_seq":${s},"through_server_seq":${s}}`) as {
      results: { server_seq: bigint }[];
      head_server_seq: bigint;
      through_server_seq: bigint;
    };
    expect(v.head_server_seq).toBe(BigInt(s));
    expect(v.through_server_seq).toBe(BigInt(s));
    expect(v.results[0]!.server_seq).toBe(BigInt(s));
    expect(String(v.head_server_seq)).toBe(s);
  });

  it("JSON.parse would have corrupted the value above 2^53 (why this parser exists)", () => {
    expect(String((JSON.parse('{"a":9007199254740993}') as { a: number }).a)).not.toBe("9007199254740993");
    expect(String((parseLossless('{"head_server_seq":9007199254740993}') as { head_server_seq: bigint }).head_server_seq)).toBe("9007199254740993");
  });

  it("keeps non-sequence numbers as numbers and matches JSON.parse on ordinary documents", () => {
    const text = JSON.stringify(snapshotJson);
    expect(parseLossless(text)).toEqual(JSON.parse(text));
    expect(parseLossless('{"x":1.5e3,"y":-0.25,"s":"a\\u00e9\\n","n":null,"b":[true,false]}')).toEqual({ x: 1500, y: -0.25, s: "aé\n", n: null, b: [true, false] });
  });

  it("refuses non-integer or negative sequence literals and malformed JSON", () => {
    for (const bad of ['{"server_seq":1.0}', '{"server_seq":1e3}', '{"server_seq":-1}', '{"head_server_seq":"1"}'.replace('"1"', "01x")]) {
      expect(() => parseLossless(bad)).toThrow(JsonSyntaxError);
    }
    for (const bad of ["", "{", '{"a":1,}', '{"a":1}x', "[1 2]", '{"a":1,"a":2}', "NaN"]) {
      expect(() => parseLossless(bad)).toThrow(JsonSyntaxError);
    }
  });

  it("the read API delivers heads and snapshots above 2^53 and at u64::MAX without rounding", async () => {
    for (const s of ["9007199254740993", "18446744073709551615"]) {
      const lib = manifest.library_id;
      const fetchFn: FetchLike = async (url) =>
        new Response(
          url.endsWith("/snapshot")
            ? `{"library_id":"${lib}","through_server_seq":${s},"snapshot_sha256":"${manifest.snapshot_sha256}","snapshot":${JSON.stringify(snapshotJson)}}`
            : `{"library_id":"${lib}","genesis_sha256":"${manifest.snapshot_sha256}","head_server_seq":${s},"snapshot_sha256":"${manifest.snapshot_sha256}"}`,
        );
      const api = createReadOnlyApi(null, fetchFn);
      expect((await api.libraryHead(lib)).head_server_seq).toBe(BigInt(s));
      expect((await api.snapshot(lib)).through_server_seq).toBe(BigInt(s));
    }
  });
});

describe("u64 client sequences (SEQ20)", () => {
  it("SEQ20 is order-preserving and exact over the whole range", () => {
    const vals = BIG.map(BigInt);
    const enc = vals.map(toSeq20);
    expect(enc.every((e) => e.length === 20)).toBe(true);
    expect([...enc].sort()).toEqual(enc);
    expect(enc.map(fromSeq20)).toEqual(vals);
    expect(toSeq20(1n)).toBe("00000000000000000001");
    expect(toSeq20(U64_MAX)).toBe("18446744073709551615");
  });
  it("refuses out-of-range and malformed text", () => {
    expect(() => toSeq20(-1n)).toThrow(SequenceError);
    expect(() => toSeq20(U64_MAX + 1n)).toThrow(SequenceError);
    for (const bad of ["18446744073709551616", "", "1.0", "-1", "1e3", " 1", "123456789012345678901"]) {
      expect(() => fromSeq20(bad)).toThrow(SequenceError);
    }
  });
  it("never wraps: exhaustion at u64::MAX", () => {
    expect(nextSeq(U64_MAX - 1n)).toBe(U64_MAX);
    expect(nextSeq(U64_MAX)).toBeNull();
    expect(nextSeq(9007199254740992n)).toBe(9007199254740993n);
  });
});
