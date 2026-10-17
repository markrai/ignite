import { describe, expect, it } from "vitest";
import { MAX_STROKE_POINTS, StrokeCapture, toCanonical } from "../src/authoring/capture";
import { createReplicaApi, RefusedError, ReplicaUnauthorizedError, TransientError, type OperationAck } from "../src/api/replicaApi";
import type { FetchLike } from "../src/api/readOnlyApi";
import { layout, screenTransform, toScreen, type ScreenTransform } from "../src/rendering/viewport";
import { LIB } from "./replicaHelpers";

const PAGE = { width: 15624, height: 20832 };

describe("pointer → canonical coordinates", () => {
  const cases: [string, { kind: "fit" } | { kind: "zoom"; factor: number }, number, number][] = [
    ["fit", { kind: "fit" }, 0, 0],
    ["zoom 250% scrolled", { kind: "zoom", factor: 2.5 }, 731, 1904],
    ["zoom 25%", { kind: "zoom", factor: 0.25 }, 0, 0],
    ["zoom 400% scrolled far", { kind: "zoom", factor: 4 }, 4000, 9000],
  ];
  it.each(cases)("%s: toCanonical inverts toScreen", (_n, mode, sl, st) => {
    const lay = layout({ width: 1280, height: 900 }, PAGE, mode);
    const t = screenTransform(lay, sl, st);
    for (const [x, y] of [
      [0, 0],
      [15624, 20832],
      [7812.5, 10416.25],
      [123.25, 20000],
    ] as const) {
      const [sx, sy] = toScreen(t, x, y);
      const c = toCanonical(t, sx, sy);
      expect(c.x).toBeCloseTo(x, 6);
      expect(c.y).toBeCloseTo(y, 6);
    }
  });
  it("is independent of devicePixelRatio (CSS px in, canonical out)", () => {
    // DPR only scales the backing store; the transform and the pointer both live in CSS px.
    const lay = layout({ width: 800, height: 1000 }, PAGE, { kind: "fit" });
    const t = screenTransform(lay, 0, 0);
    const [sx, sy] = toScreen(t, 1000, 2000);
    expect(toCanonical(t, sx, sy).x).toBeCloseTo(1000, 6);
  });
});

function tIdentity(): ScreenTransform {
  return { scale: 1, offsetX: 0, offsetY: 0 };
}
const s = (x: number, y: number, pressure = 0.5) => ({ localX: x, localY: y, pressure });

describe("stroke capture", () => {
  it("a stroke must start on the page", () => {
    expect(StrokeCapture.begin("pen", tIdentity(), PAGE, s(-1, 10))).toBeNull();
    expect(StrokeCapture.begin("pen", tIdentity(), PAGE, s(10, 20833))).toBeNull();
    expect(StrokeCapture.begin("mouse", tIdentity(), PAGE, s(0, 0))).not.toBeNull();
    expect(StrokeCapture.begin("mouse", tIdentity(), PAGE, s(15624, 20832))).not.toBeNull();
  });

  it("later samples are clamped to the page (Kindle geometry contract)", () => {
    const c = StrokeCapture.begin("mouse", tIdentity(), PAGE, s(10, 10))!;
    c.add(tIdentity(), s(-500, 30000));
    c.add(tIdentity(), s(99999, -3));
    const r = c.finish(tIdentity(), null);
    expect(r.kind === "stroke" && r.stroke.points).toEqual([
      { x: 10, y: 10 },
      { x: 0, y: 20832 },
      { x: 15624, y: 0 },
    ]);
  });

  it("pen → PressureWidthV1 with real pressure; pointerup pressure 0 is not ink", () => {
    const c = StrokeCapture.begin("pen", tIdentity(), PAGE, s(1, 1, 0.25))!;
    c.add(tIdentity(), s(2, 2, 0.75));
    const r = c.finish(tIdentity(), s(3, 3, 0));
    expect(r.kind).toBe("stroke");
    if (r.kind !== "stroke") return;
    expect(r.stroke.style).toBe("pressure-width-v1");
    expect(r.stroke.points).toEqual([
      { x: 1, y: 1, pressure: 0.25 },
      { x: 2, y: 2, pressure: 0.75 },
    ]);
  });

  it("mouse → LegacyFixedV1 with no pressure fields", () => {
    const c = StrokeCapture.begin("mouse", tIdentity(), PAGE, s(1, 1, 0.5))!;
    c.add(tIdentity(), s(2, 2, 0.5));
    const r = c.finish(tIdentity(), s(3, 3, 0));
    expect(r.kind === "stroke" && r.stroke).toEqual({ style: "legacy-fixed-v1", points: [{ x: 1, y: 1 }, { x: 2, y: 2 }, { x: 3, y: 3 }] });
  });

  it("a page without a pressure curve gets LegacyFixedV1 even from a pen", () => {
    const c = StrokeCapture.begin("pen", tIdentity(), PAGE, s(1, 1, 0.5), false)!;
    const r = c.finish(tIdentity(), null);
    expect(r.kind === "stroke" && r.stroke.style).toBe("legacy-fixed-v1");
  });

  it("drops exact consecutive duplicates only; no smoothing; f32 values", () => {
    const c = StrokeCapture.begin("pen", tIdentity(), PAGE, s(1.1, 1.1, 0.3))!;
    c.add(tIdentity(), s(1.1, 1.1, 0.3));
    c.add(tIdentity(), s(1.1, 1.1, 0.4)); // same place, new pressure: kept
    c.add(tIdentity(), s(5, 5, 0.4));
    c.add(tIdentity(), s(1.1, 1.1, 0.3)); // not consecutive: kept
    const r = c.finish(tIdentity(), null);
    if (r.kind !== "stroke") throw new Error("expected stroke");
    expect(r.stroke.points).toHaveLength(4);
    expect(r.stroke.points[0]!.x).toBe(Math.fround(1.1));
    expect(r.stroke.points[0]!.pressure).toBe(Math.fround(0.3));
  });

  it("a single tap is a one-point stroke", () => {
    const c = StrokeCapture.begin("mouse", tIdentity(), PAGE, s(50, 60))!;
    const r = c.finish(tIdentity(), s(50, 60));
    expect(r.kind === "stroke" && r.stroke.points).toEqual([{ x: 50, y: 60 }]);
  });

  it("refuses an oversize stroke clearly instead of truncating it", () => {
    const c = StrokeCapture.begin("mouse", tIdentity(), PAGE, s(0, 0))!;
    for (let i = 1; i <= MAX_STROKE_POINTS + 5; i++) c.add(tIdentity(), s(i % 15000, Math.floor(i / 15000)));
    expect(c.tooLong).toBe(true);
    expect(c.finish(tIdentity(), null).kind).toBe("too-long");
  });
});

describe("replica API (POST-only)", () => {
  const env = (op: string) => `{"v":1,"operation_id":"${op}","x":1}`;
  const OP1 = "01a0c450-6c03-7301-8203-040506070803";
  const OP2 = "01a0c450-6c03-7301-8203-040506070804";

  function fake(status: number, body: string) {
    const seen: { url: string; init: RequestInit }[] = [];
    const fetchFn: FetchLike = async (url, init) => {
      seen.push({ url, init });
      return new Response(body, { status });
    };
    return { seen, fetchFn };
  }
  const ack = (seqs: string[], ops = [OP1, OP2]) =>
    `{"library_id":"${LIB}","head_server_seq":${seqs[seqs.length - 1]},"results":[${seqs
      .map((q, i) => `{"operation_id":"${ops[i]}","server_seq":${q},"outcome":"applied","duplicate":false}`)
      .join(",")}]}`;

  it("sends the exact stored bytes as one same-origin POST with the token only in the header", async () => {
    const f = fake(200, ack(["9007199254740993", "18446744073709551615"]));
    const api = createReplicaApi("t0ken", f.fetchFn);
    const r = await api.submit(LIB, [
      { operation_id: OP1, envelope: env(OP1) },
      { operation_id: OP2, envelope: env(OP2) },
    ]);
    expect(f.seen).toHaveLength(1);
    expect(f.seen[0]!.url).toBe(`/v1/libraries/${LIB}/operations`);
    expect(f.seen[0]!.init.method).toBe("POST");
    expect(f.seen[0]!.init.body).toBe(`{"envelopes":[${env(OP1)},${env(OP2)}]}`);
    expect((f.seen[0]!.init.headers as Record<string, string>).Authorization).toBe("Bearer t0ken");
    expect(f.seen[0]!.url).not.toContain("t0ken");
    expect(r.results.map((x: OperationAck) => x.server_seq)).toEqual([9007199254740993n, 18446744073709551615n]);
    expect(r.head_server_seq).toBe(18446744073709551615n);
  });

  it("classifies failures: transient / unauthorized / refused", async () => {
    const one = [{ operation_id: OP1, envelope: env(OP1) }];
    const run = (status: number, body = "{}") => createReplicaApi(null, fake(status, body).fetchFn).submit(LIB, one);
    await expect(run(503)).rejects.toBeInstanceOf(TransientError);
    await expect(run(429)).rejects.toBeInstanceOf(TransientError);
    await expect(run(401)).rejects.toBeInstanceOf(ReplicaUnauthorizedError);
    const e = await run(422, '{"error":"semantic_refusal","semantic_code":"unknown_page","envelope_index":0,"message":"no page"}').catch((x: unknown) => x);
    expect(e).toBeInstanceOf(RefusedError);
    expect((e as RefusedError).code).toBe("unknown_page");
    expect((e as RefusedError).envelopeIndex).toBe(0);
    await expect(run(409, '{"error":"client_seq_out_of_order","message":"gap"}')).rejects.toBeInstanceOf(RefusedError);
    await expect(createReplicaApi(null, async () => Promise.reject(new Error("net"))).submit(LIB, one)).rejects.toBeInstanceOf(TransientError);
    // A truncated 200 is a lost ACK (retry), not a conflict.
    await expect(run(200, '{"library_id":')).rejects.toBeInstanceOf(TransientError);
  });

  it("refuses an ACK that does not match what was sent", async () => {
    const one = [{ operation_id: OP1, envelope: env(OP1) }];
    await expect(createReplicaApi(null, fake(200, ack(["1"], [OP2])).fetchFn).submit(LIB, one)).rejects.toBeInstanceOf(RefusedError);
    await expect(createReplicaApi(null, fake(200, ack(["1", "2"])).fetchFn).submit(LIB, one)).rejects.toBeInstanceOf(RefusedError);
  });

  it("exposes nothing but submit", () => {
    expect(Object.keys(createReplicaApi(null, fake(200, "{}").fetchFn))).toEqual(["submit"]);
  });
});
