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
