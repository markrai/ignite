import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { CANONICAL_PAGE_HEIGHT, CANONICAL_PAGE_WIDTH } from "../src/model/generated/protocolConstants";
import {
  ProtocolViolation,
  validateLibraryHead,
  validateLibraryList,
  validateSnapshot,
  validateSnapshotResponse,
} from "../src/model/validate";
import { clampTextOrigin, placeTextBox } from "../src/authoring/textDraft";
import { clone, manifest, snapshotBytes, snapshotJson } from "./fixtures";

type J = Record<string, any>; // eslint-disable-line @typescript-eslint/no-explicit-any

describe("Rust-produced SyncSnapshotV1 fixture", () => {
  it("is the pinned canonical bytes", () => {
    expect(createHash("sha256").update(snapshotBytes).digest("hex")).toBe(manifest.snapshot_sha256);
  });

  it("validates and keeps canonical order and content", () => {
    const s = validateSnapshot(snapshotJson);
    expect(s.library_id).toBe(manifest.library_id);
    expect(s.page_space).toEqual({ width: CANONICAL_PAGE_WIDTH, height: CANONICAL_PAGE_HEIGHT });
    expect(s.notebooks.map((n) => n.title)).toEqual(["Field Notes", "Sketches"]);
    expect(s.notebooks.map((n) => n.pages.length)).toEqual([3, 2]);
    const all = s.notebooks.flatMap((n) => n.pages).flatMap((p) => p.strokes);
    expect(all.some((x) => x.style === "legacy-fixed-v1")).toBe(true);
    expect(all.some((x) => x.style === "pressure-width-v1")).toBe(true);
    expect(all.some((x) => x.root_id !== x.stroke_id)).toBe(true);
    const bgs = s.notebooks.flatMap((n) => n.pages).map((p) => p.background.kind);
    expect(bgs).toContain("blank");
    expect(bgs).toContain("blob");
    // Order is preserved exactly as sent.
    const raw = snapshotJson as J;
    expect(s.notebooks[0]!.pages[0]!.strokes.map((x) => x.stroke_id)).toEqual(
      raw.notebooks[0].pages[0].strokes.map((x: J) => x.stroke_id),
    );
  });

  it("partial-erase fragments share the erased stroke's root lineage", () => {
    const s = validateSnapshot(snapshotJson);
    const frags = s.notebooks[0]!.pages[0]!.strokes.filter((x) => x.root_id !== x.stroke_id);
    expect(frags.length).toBe(2);
    expect(new Set(frags.map((f) => f.root_id)).size).toBe(1);
  });

  it("omitted page links become an empty array", () => {
    const s = validateSnapshot(snapshotJson);
    expect(s.notebooks.every((n) => n.pages.every((p) => p.links.length === 0))).toBe(true);
    const raw = snapshotJson as J;
    expect(raw.notebooks[0].pages[0].links).toBeUndefined();
  });

  it("accepts a well-formed page link and refuses a self-link", () => {
    const j = clone(snapshotJson) as J;
    const nb = j.notebooks[0];
    const src = nb.pages[0];
    const dst = nb.pages[1];
    const link = {
      link_id: "01a0c450-6c14-7401-8203-04050607081a",
      source_notebook_id: nb.notebook_id,
      source_page_id: src.page_id,
      source_x: 100,
      source_y: 200,
      target_notebook_id: nb.notebook_id,
      target_page_id: dst.page_id,
    };
    src.links = [link];
    const s = validateSnapshot(j);
    expect(s.notebooks[0]!.pages[0]!.links).toEqual([link]);

    const self = clone(snapshotJson) as J;
    self.notebooks[0].pages[0].links = [
      {
        ...link,
        source_notebook_id: self.notebooks[0].notebook_id,
        source_page_id: self.notebooks[0].pages[0].page_id,
        target_notebook_id: self.notebooks[0].notebook_id,
        target_page_id: self.notebooks[0].pages[0].page_id,
      },
    ];
    expect(() => validateSnapshot(self)).toThrow(ProtocolViolation);
  });
});

describe("snapshot validation refuses instead of partially rendering", () => {
  const mutate = (f: (j: J) => void) => {
    const j = clone(snapshotJson) as J;
    f(j);
    return () => validateSnapshot(j);
  };
  it.each<[string, (j: J) => void]>([
    ["unsupported version", (j) => (j.v = 2)],
    ["other page space", (j) => (j.page_space.width = 1860)],
    ["unknown top-level field", (j) => (j.extra = 1)],
    ["unknown stroke field", (j) => (j.notebooks[0].pages[0].strokes[0].t_us = 5)],
    ["bad uuid", (j) => (j.library_id = "not-a-uuid")],
    ["uppercase uuid", (j) => (j.library_id = String(j.library_id).toUpperCase())],
    ["empty title", (j) => (j.notebooks[0].title = "  ")],
    ["no pages", (j) => (j.notebooks[0].pages = [])],
    ["point off page", (j) => (j.notebooks[0].pages[0].strokes[0].points[0].x = 99999)],
    ["pressure > 1", (j) => (j.notebooks[0].pages[0].strokes[1].points[0].pressure = 1.5)],
    ["no points", (j) => (j.notebooks[0].pages[0].strokes[0].points = [])],
    ["unknown style", (j) => (j.notebooks[0].pages[0].strokes[0].style = "brush-x")],
    ["unknown background kind", (j) => (j.notebooks[0].pages[0].background = { kind: "png" })],
    ["unsupported encoding", (j) => (j.notebooks[1].pages[0].background.format.encoding = "rgba8")],
    ["byte_len mismatch", (j) => (j.notebooks[1].pages[0].background.byte_len += 1)],
    ["stride < width", (j) => (j.notebooks[1].pages[0].background.format.stride = 1)],
    ["bad ink", (j) => (j.notebooks[0].pages[0].render.ink_byte = 7)],
    ["pressure stroke without curve", (j) => delete j.notebooks[0].pages[0].render.pressure],
    ["duplicate stroke", (j) => j.notebooks[0].pages[0].strokes.push(clone(j.notebooks[0].pages[0].strokes[0]))],
    ["not an object", (j) => (j.notebooks = 5)],
  ])("%s", (_, f) => {
    expect(mutate(f)).toThrow(ProtocolViolation);
  });
});

describe("library list / head / snapshot response", () => {
  const row = {
    library_id: manifest.library_id,
    head_server_seq: 26n,
    snapshot_sha256: manifest.snapshot_sha256,
    notebook_count: 2,
    page_count: 5,
  };
  it("parses empty and populated lists", () => {
    expect(validateLibraryList({ v: 1, libraries: [] })).toEqual([]);
    expect(validateLibraryList({ v: 1, libraries: [row] })).toEqual([row]);
  });
  it("refuses malformed lists", () => {
    expect(() => validateLibraryList({ v: 2, libraries: [] })).toThrow(ProtocolViolation);
    expect(() => validateLibraryList({ libraries: [] })).toThrow(ProtocolViolation);
    expect(() => validateLibraryList({ v: 1, libraries: [{ ...row, title: "invented" }] })).toThrow(ProtocolViolation);
    expect(() => validateLibraryList({ v: 1, libraries: [{ ...row, head_server_seq: -1n }] })).toThrow(ProtocolViolation);
    expect(() => validateLibraryList({ v: 1, libraries: [{ ...row, snapshot_sha256: "ABC" }] })).toThrow(ProtocolViolation);
  });
  it("parses the poll head", () => {
    const head = { library_id: manifest.library_id, genesis_sha256: manifest.snapshot_sha256, head_server_seq: 3n, snapshot_sha256: manifest.snapshot_sha256 };
    expect(validateLibraryHead(head)).toEqual(head);
    expect(() => validateLibraryHead({ ...head, head_server_seq: 1.5 })).toThrow(ProtocolViolation);
    expect(() => validateLibraryHead({ ...head, head_server_seq: 3 })).toThrow(ProtocolViolation);
    expect(() => validateLibraryHead({ ...head, head_server_seq: 18446744073709551616n })).toThrow(ProtocolViolation);
    expect(validateLibraryHead({ ...head, head_server_seq: 18446744073709551615n }).head_server_seq).toBe(18446744073709551615n);
  });
  it("checks the snapshot response envelope and library identity", () => {
    const resp = { library_id: manifest.library_id, through_server_seq: 0n, snapshot_sha256: manifest.snapshot_sha256, snapshot: snapshotJson };
    expect(validateSnapshotResponse(resp, manifest.library_id).through_server_seq).toBe(0n);
    expect(() => validateSnapshotResponse(resp, "01a0da0a-891a-76f8-b048-caee359ef057")).toThrow(ProtocolViolation);
  });
});

// Rust validates SyncText in f32 (`text_width_is_checked_in_f32_arithmetic` in
// sync-protocol/tests/protocol.rs pins the same values). The server writes each f32 as its
// shortest round-tripping decimal; the Web parses that as f64 and must still agree with Rust.
describe("text geometry is validated in Rust's f32 arithmetic", () => {
  const f32 = Math.fround;
  /** The server's canonical JSON number for an f32 (shortest decimal that round-trips). */
  const canonicalF32 = (v: number): string => {
    for (let p = 1; p <= 9; p++) {
      const s = String(Number(v.toPrecision(p)));
      if (f32(Number(s)) === v) return s;
    }
    throw new Error(`not an f32: ${v}`);
  };
  const withText = (geom: { x: string; y: string; width: string }) => {
    const j = clone(snapshotJson) as J;
    const page = j.notebooks[0].pages[0];
    const text =
      `{"text_id":"01a0c450-6c14-7401-8203-04050607081b","x":${geom.x},"y":${geom.y},` +
      `"width":${geom.width},"font_size":340,"face":"default-sans-v1","content":"hello"}`;
    page.texts = [JSON.parse(text)];
    return () => validateSnapshot(JSON.parse(JSON.stringify(j)));
  };
  const nextF32 = (v: number) => {
    const b = new Float32Array([v]);
    new Uint32Array(b.buffer)[0]! += 1;
    return b[0]!;
  };

  it("accepts the reported box: x=6500.0005 (f32 6500.00048828125), width 9124", () => {
    expect(f32(6500.0005)).toBe(6500.00048828125);
    // Exact f64 remaining width is 9123.99951171875; the f32 subtraction rounds the tie to 9124.
    expect(CANONICAL_PAGE_WIDTH - 6500.00048828125).toBe(9123.99951171875);
    expect(f32(CANONICAL_PAGE_WIDTH - f32(6500.0005))).toBe(9124);
    const s = withText({ x: "6500.0005", y: "100", width: "9124" })();
    expect(s.notebooks[0]!.pages[0]!.texts[0]!.width).toBe(9124);
  });

  it.each<[string, { x: string; y: string; width: string }]>([
    ["the next f32 above the remaining width", { x: "6500.0005", y: "100", width: canonicalF32(nextF32(9124)) }],
    ["a clearly overflowing width", { x: "6500.0005", y: "100", width: "9125" }],
    ["a width wider than the page", { x: "0", y: "100", width: canonicalF32(nextF32(CANONICAL_PAGE_WIDTH)) }],
    ["a zero width", { x: "100", y: "100", width: "0" }],
    ["a negative width", { x: "100", y: "100", width: "-1" }],
    ["an origin on the right edge", { x: String(CANONICAL_PAGE_WIDTH), y: "100", width: "1" }],
    ["an origin on the bottom edge", { x: "100", y: String(CANONICAL_PAGE_HEIGHT), width: "1" }],
  ])("refuses %s", (_, geom) => {
    expect(withText(geom)).toThrow(ProtocolViolation);
  });

  it("keeps accepting ordinary text geometry", () => {
    expect(withText({ x: "100", y: "200", width: "5000" })).not.toThrow();
    expect(withText({ x: "0", y: "0", width: String(CANONICAL_PAGE_WIDTH) })).not.toThrow();
    expect(withText({ x: "15623", y: "20831", width: "1" })).not.toThrow();
  });

  it("every box placed near the right edge survives canonical JSON and validation", () => {
    for (let i = 0; i <= 4000; i++) {
      const x = 5000 + (i * (CANONICAL_PAGE_WIDTH - 5000)) / 4000.5;
      const box = placeTextBox(x, 1234.5);
      // As the server writes it (shortest f32 decimals) and as JSON.stringify writes the f32s.
      for (const fmt of [canonicalF32, (v: number) => JSON.stringify(v)]) {
        expect(withText({ x: fmt(box.x), y: fmt(box.y), width: fmt(box.width) }), `x=${x}`).not.toThrow();
      }
    }
    // 8,002 full-snapshot validations exceed the 5 s default on slower CI runners.
  }, 20_000);

  it("an anchor that rounds onto the page edge as f32 still yields a valid box", () => {
    const box = placeTextBox(CANONICAL_PAGE_WIDTH - 1e-4, CANONICAL_PAGE_HEIGHT - 1e-4);
    expect(box.x).toBeLessThan(CANONICAL_PAGE_WIDTH);
    expect(box.y).toBeLessThan(CANONICAL_PAGE_HEIGHT);
    expect(box.width).toBeGreaterThan(0);
    const fmt = (v: number) => canonicalF32(v);
    expect(withText({ x: fmt(box.x), y: fmt(box.y), width: fmt(box.width) })).not.toThrow();
  });

  it("a box dragged past the right edge is clamped to geometry Rust accepts", () => {
    for (let i = 0; i <= 4000; i++) {
      const width = f32(1 + (i * 15_000) / 4000.25);
      const c = clampTextOrigin(CANONICAL_PAGE_WIDTH + 500, 1234.5, width);
      expect(f32(width) <= f32(CANONICAL_PAGE_WIDTH - f32(c.x)), `width=${width}`).toBe(true);
      expect(
        withText({ x: canonicalF32(c.x), y: canonicalF32(c.y), width: canonicalF32(width) }),
        `width=${width}`,
      ).not.toThrow();
    }
  });
});
