import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { BridgeError, ProtocolBridge } from "../src/wasm/bridge";

// The Rust test `native/scribe-web-wasm/tests/bridge.rs` pins this fixture against independently
// constructed canonical envelopes; here the *built* `.wasm` the browser ships must reproduce every
// case byte-for-byte.
interface Case {
  name: string;
  request: unknown;
  ok?: string;
  err?: boolean;
}
const fixture = JSON.parse(readFileSync(new URL("./fixtures/wasm-envelopes-v1.json", import.meta.url), "utf8")) as { v: number; cases: Case[] };
const wasm = readFileSync(new URL("../src/wasm/scribe_web_wasm.wasm", import.meta.url));

async function bridge() {
  return ProtocolBridge.instantiate(new Uint8Array(wasm));
}

const S1 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

describe("WASM protocol bridge (shipped binary)", () => {
  it("fixture is non-trivial", () => {
    expect(fixture.v).toBe(1);
    expect(fixture.cases.length).toBeGreaterThan(20);
  });

  it.each(fixture.cases.map((c) => [c.name, c] as const))("%s — identical to Rust", async (_n, c) => {
    const r = (await bridge()).call(c.request);
    if (c.ok !== undefined) {
      expect(r.err).toBeUndefined();
      expect(r.ok).toBe(c.ok);
    } else {
      expect(r.ok).toBeUndefined();
      expect(typeof r.err).toBe("string");
      expect(r.err!.length).toBeGreaterThan(0);
    }
  });

  it("the typed encoder produces the fixture bytes (client_seq as text; never a JS number)", async () => {
    const b = await bridge();
    for (const c of fixture.cases) {
      const req = c.request as { fn: string } & Record<string, unknown>;
      if (req.fn !== "encode_add_stroke" || c.ok === undefined) continue;
      const rest = Object.fromEntries(Object.entries(req).filter(([k]) => k !== "fn"));
      expect(b.encodeAddStroke(rest as never)).toBe(c.ok);
    }
  });

  it("mints S1 UUIDv7 identities from crypto randomness", async () => {
    const b = await bridge();
    const ids = new Set<string>();
    for (const k of ["replica", "operation", "stroke", "folder", "text", "notebook", "page"] as const) {
      for (let i = 0; i < 50; i++) {
        const id = b.mintId(k);
        expect(id).toMatch(S1);
        ids.add(id);
      }
    }
    expect(ids.size).toBe(350);
    const fixed = b.mintId("stroke", (a) => a.fill(0xab), () => 1_790_000_000_000);
    expect(fixed).toMatch(S1);
    expect(fixed.startsWith("01a0c450-6c00-7")).toBe(true);
  });

  it("refuses anything but AddStroke and reports errors as BridgeError", async () => {
    const b = await bridge();
    expect(b.call({ fn: "encode_erase_region" }).err).toBeTruthy();
    expect(() =>
      b.encodeAddStroke({
        library_id: "x",
        replica_id: "x",
        client_seq: "1",
        operation_id: "x",
        notebook_id: "x",
        page_id: "x",
        stroke_id: "x",
        style: "legacy-fixed-v1",
        points: [{ x: 1, y: 1 }],
      }),
    ).toThrow(BridgeError);
  });
});
