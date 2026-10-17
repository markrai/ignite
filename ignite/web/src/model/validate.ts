// Strict runtime validation of server responses (mirrors `sync-protocol` validation rules).
// Any violation throws `ProtocolViolation`; callers show an error instead of rendering.

import {
  CANONICAL_PAGE_HEIGHT,
  CANONICAL_PAGE_WIDTH,
  SNAPSHOT_VERSION,
  STYLE_LEGACY_FIXED_V1,
  STYLE_PRESSURE_WIDTH_V1,
} from "./generated/protocolConstants";
import type {
  BackgroundRef,
  LibraryHead,
  LibrarySummary,
  PressureCurveRef,
  RenderProfileRef,
  SnapshotFolder,
  SnapshotNotebook,
  SnapshotPage,
  SnapshotResponse,
  SyncPoint,
  SyncSnapshotV1,
  SyncStroke,
  SyncText,
  SyncPageLink,
} from "./types";

export class ProtocolViolation extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolViolation";
  }
}

const MAX_TITLE_CHARS = 96;
const MAX_STROKE_POINTS = 100_000;
const MAX_TEXT_CHARS = 8192;

type Obj = Record<string, unknown>;

function fail(where: string, what: string): never {
  throw new ProtocolViolation(`${where}: ${what}`);
}

function obj(v: unknown, where: string, required: string[], optional: string[] = []): Obj {
  if (typeof v !== "object" || v === null || Array.isArray(v)) fail(where, "expected an object");
  const o = v as Obj;
  for (const k of required) if (!(k in o)) fail(where, `missing field "${k}"`);
  for (const k of Object.keys(o)) {
    if (!required.includes(k) && !optional.includes(k)) fail(where, `unknown field "${k}"`);
  }
  return o;
}

function arr(v: unknown, where: string): unknown[] {
  if (!Array.isArray(v)) fail(where, "expected an array");
  return v;
}

function str(v: unknown, where: string): string {
  if (typeof v !== "string") fail(where, "expected a string");
  return v;
}

function num(v: unknown, where: string): number {
  if (typeof v !== "number" || !Number.isFinite(v)) fail(where, "expected a finite number");
  return v;
}

function int(v: unknown, where: string, min: number, max: number): number {
  const n = num(v, where);
  if (!Number.isInteger(n) || n < min || n > max) fail(where, `expected integer in ${min}..=${max}`);
  return n;
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-([457])[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const SHA_RE = /^[0-9a-f]{64}$/;

export function uuid(v: unknown, where: string): string {
  const s = str(v, where);
  if (!UUID_RE.test(s)) fail(where, `invalid UUID ${JSON.stringify(s)} (need canonical v4/v5/v7)`);
  return s;
}

export function sha256(v: unknown, where: string): string {
  const s = str(v, where);
  if (!SHA_RE.test(s)) fail(where, "invalid sha256 (need 64 lowercase hex)");
  return s;
}

export function validateTitle(title: string, where: string): string {
  if (title.trim() === "") fail(where, "empty title");
  if ([...title].length > MAX_TITLE_CHARS) fail(where, `title longer than ${MAX_TITLE_CHARS}`);
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f]/u.test(title)) fail(where, "control characters in title");
  return title;
}

function point(v: unknown, where: string): SyncPoint {
  const o = obj(v, where, ["x", "y"], ["pressure"]);
  const x = num(o.x, `${where}.x`);
  const y = num(o.y, `${where}.y`);
  if (x < 0 || x > CANONICAL_PAGE_WIDTH || y < 0 || y > CANONICAL_PAGE_HEIGHT) {
    fail(where, "point outside the canonical page");
  }
  if ("pressure" in o) {
    const p = num(o.pressure, `${where}.pressure`);
    if (p < 0 || p > 1) fail(where, "pressure outside 0..=1");
    return { x, y, pressure: p };
  }
  return { x, y };
}

function stroke(v: unknown, where: string): SyncStroke {
  const o = obj(v, where, ["stroke_id", "root_id", "style", "points"]);
  const style = str(o.style, `${where}.style`);
  if (style !== STYLE_LEGACY_FIXED_V1 && style !== STYLE_PRESSURE_WIDTH_V1) {
    fail(where, `unsupported stroke style ${JSON.stringify(style)}`);
  }
  const pts = arr(o.points, `${where}.points`);
  if (pts.length === 0) fail(where, "stroke has no points");
  if (pts.length > MAX_STROKE_POINTS) fail(where, "too many points");
  return {
    stroke_id: uuid(o.stroke_id, `${where}.stroke_id`),
    root_id: uuid(o.root_id, `${where}.root_id`),
    style,
    points: pts.map((p, i) => point(p, `${where}.points[${i}]`)),
  };
}

function textObj(v: unknown, where: string): SyncText {
  const o = obj(v, where, ["text_id", "x", "y", "width", "font_size", "face", "content"]);
  const x = num(o.x, `${where}.x`);
  const y = num(o.y, `${where}.y`);
  const width = num(o.width, `${where}.width`);
  const font_size = num(o.font_size, `${where}.font_size`);
  if (!(x >= 0 && x < CANONICAL_PAGE_WIDTH) || !(y >= 0 && y < CANONICAL_PAGE_HEIGHT)) {
    fail(where, "text origin outside the canonical page");
  }
  if (!(width > 0) || width > CANONICAL_PAGE_WIDTH - x) fail(where, "text width out of range");
  if (!(font_size >= 1) || font_size > CANONICAL_PAGE_HEIGHT) fail(where, "invalid font_size");
  if (str(o.face, `${where}.face`) !== "default-sans-v1") fail(where, "unsupported text face");
  const content = str(o.content, `${where}.content`);
  if (content === "") fail(where, "empty text content");
  if ([...content].length > MAX_TEXT_CHARS) fail(where, `text longer than ${MAX_TEXT_CHARS}`);
  if (content.includes("\r")) fail(where, "CR must be normalized to LF");
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u0009\u000b-\u001f\u007f-\u009f]/u.test(content)) fail(where, "control characters in text");
  return { text_id: uuid(o.text_id, `${where}.text_id`), x, y, width, font_size, face: "default-sans-v1", content };
}

function pageLink(v: unknown, where: string): SyncPageLink {
  const o = obj(v, where, [
    "link_id",
    "source_notebook_id",
    "source_page_id",
    "source_x",
    "source_y",
    "target_notebook_id",
    "target_page_id",
  ]);
  const source_x = num(o.source_x, `${where}.source_x`);
  const source_y = num(o.source_y, `${where}.source_y`);
  if (!(source_x >= 0 && source_x < CANONICAL_PAGE_WIDTH) || !(source_y >= 0 && source_y < CANONICAL_PAGE_HEIGHT)) {
    fail(where, "link origin outside the canonical page");
  }
  const source_notebook_id = uuid(o.source_notebook_id, `${where}.source_notebook_id`);
  const source_page_id = uuid(o.source_page_id, `${where}.source_page_id`);
  const target_notebook_id = uuid(o.target_notebook_id, `${where}.target_notebook_id`);
  const target_page_id = uuid(o.target_page_id, `${where}.target_page_id`);
  if (source_notebook_id === target_notebook_id && source_page_id === target_page_id) {
    fail(where, "link cannot target its source page");
  }
  return {
    link_id: uuid(o.link_id, `${where}.link_id`),
    source_notebook_id,
    source_page_id,
    source_x,
    source_y,
    target_notebook_id,
    target_page_id,
  };
}

function background(v: unknown, where: string): BackgroundRef {
  const kind = str(obj(v, where, ["kind"], ["sha256", "byte_len", "format"]).kind, `${where}.kind`);
  if (kind === "blank") {
    obj(v, where, ["kind"]);
    return { kind: "blank" };
  }
  if (kind !== "blob") fail(where, `unsupported background kind ${JSON.stringify(kind)}`);
  const o = obj(v, where, ["kind", "sha256", "byte_len", "format"]);
  const f = obj(o.format, `${where}.format`, ["encoding", "width", "height", "stride"]);
  const encoding = str(f.encoding, `${where}.format.encoding`);
  if (encoding !== "gray8") fail(where, `unsupported background encoding ${JSON.stringify(encoding)}`);
  const width = int(f.width, `${where}.format.width`, 1, 0xffff_ffff);
  const height = int(f.height, `${where}.format.height`, 1, 0xffff_ffff);
  const stride = int(f.stride, `${where}.format.stride`, 1, 0xffff_ffff);
  if (stride < width) fail(where, "stride < width");
  const byteLen = int(o.byte_len, `${where}.byte_len`, 0, Number.MAX_SAFE_INTEGER);
  if (byteLen !== stride * height) fail(where, "byte_len != stride*height");
  return {
    kind: "blob",
    sha256: sha256(o.sha256, `${where}.sha256`),
    byte_len: byteLen,
    format: { encoding: "gray8", width, height, stride },
  };
}

function asciiId(v: unknown, where: string): string {
  const s = str(v, where);
  if (s.length === 0 || s.length > 64 || !/^[\x21-\x7e]+$/.test(s)) fail(where, "invalid identifier");
  return s;
}

function render(v: unknown, where: string): RenderProfileRef {
  const o = obj(
    v,
    where,
    ["profile_version", "ink_byte", "legacy_fixed_radius", "transform_id", "rasterizer_id"],
    ["pressure"],
  );
  const ink = int(o.ink_byte, `${where}.ink_byte`, 0, 255);
  if (ink !== 0x00 && ink !== 0xff) fail(where, "ink_byte must be 0x00 or 0xff");
  const out: RenderProfileRef = {
    profile_version: int(o.profile_version, `${where}.profile_version`, 0, 0xffff_ffff),
    ink_byte: ink,
    legacy_fixed_radius: int(o.legacy_fixed_radius, `${where}.legacy_fixed_radius`, 0, 0xffff_ffff),
    transform_id: asciiId(o.transform_id, `${where}.transform_id`),
    rasterizer_id: asciiId(o.rasterizer_id, `${where}.rasterizer_id`),
  };
  if ("pressure" in o) {
    const w = `${where}.pressure`;
    const p = obj(o.pressure, w, [
      "q_r1_max",
      "q_r2_max",
      "q_r3_max",
      "min_radius",
      "max_radius",
      "missing_fallback_radius",
    ]);
    const curve: PressureCurveRef = {
      q_r1_max: int(p.q_r1_max, `${w}.q_r1_max`, 0, 0xffff),
      q_r2_max: int(p.q_r2_max, `${w}.q_r2_max`, 0, 0xffff),
      q_r3_max: int(p.q_r3_max, `${w}.q_r3_max`, 0, 0xffff),
      min_radius: int(p.min_radius, `${w}.min_radius`, 0, 0xffff_ffff),
      max_radius: int(p.max_radius, `${w}.max_radius`, 0, 0xffff_ffff),
      missing_fallback_radius: int(p.missing_fallback_radius, `${w}.missing_fallback_radius`, 0, 0xffff_ffff),
    };
    if (!(curve.q_r1_max <= curve.q_r2_max && curve.q_r2_max <= curve.q_r3_max) || curve.min_radius > curve.max_radius) {
      fail(w, "invalid pressure curve");
    }
    out.pressure = curve;
  }
  return out;
}

function page(v: unknown, where: string): SnapshotPage {
  const o = obj(v, where, ["page_id", "background", "render", "strokes"], ["texts", "links"]);
  const r = render(o.render, `${where}.render`);
  const strokes = arr(o.strokes, `${where}.strokes`).map((s, i) => stroke(s, `${where}.strokes[${i}]`));
  for (const s of strokes) {
    if (s.style === STYLE_PRESSURE_WIDTH_V1 && !r.pressure) {
      fail(where, `pressure stroke ${s.stroke_id} on a page without a pressure curve`);
    }
  }
  const texts = "texts" in o ? arr(o.texts, `${where}.texts`).map((t, i) => textObj(t, `${where}.texts[${i}]`)) : [];
  const links = "links" in o ? arr(o.links, `${where}.links`).map((l, i) => pageLink(l, `${where}.links[${i}]`)) : [];
  return {
    page_id: uuid(o.page_id, `${where}.page_id`),
    background: background(o.background, `${where}.background`),
    render: r,
    strokes,
    texts,
    links,
  };
}

function notebook(v: unknown, where: string): SnapshotNotebook {
  const o = obj(v, where, ["notebook_id", "title", "pages"], ["folder_id"]);
  const pages = arr(o.pages, `${where}.pages`).map((p, i) => page(p, `${where}.pages[${i}]`));
  if (pages.length === 0) fail(where, "notebook has no pages");
  const out: SnapshotNotebook = {
    notebook_id: uuid(o.notebook_id, `${where}.notebook_id`),
    title: validateTitle(str(o.title, `${where}.title`), `${where}.title`),
    pages,
  };
  if ("folder_id" in o) out.folder_id = uuid(o.folder_id, `${where}.folder_id`);
  return out;
}

function folder(v: unknown, where: string): SnapshotFolder {
  const o = obj(v, where, ["folder_id", "title"]);
  return {
    folder_id: uuid(o.folder_id, `${where}.folder_id`),
    title: validateTitle(str(o.title, `${where}.title`), `${where}.title`),
  };
}

/** Strict `SyncSnapshotV1` validation (never `as`-cast server JSON). */
export function validateSnapshot(v: unknown): SyncSnapshotV1 {
  const o = obj(v, "snapshot", ["v", "library_id", "page_space", "notebooks"], ["folders"]);
  if (o.v !== SNAPSHOT_VERSION) fail("snapshot", `unsupported snapshot version ${JSON.stringify(o.v)}`);
  const ps = obj(o.page_space, "snapshot.page_space", ["width", "height"]);
  if (ps.width !== CANONICAL_PAGE_WIDTH || ps.height !== CANONICAL_PAGE_HEIGHT) {
    fail("snapshot.page_space", `unsupported page space ${String(ps.width)}x${String(ps.height)}`);
  }
  const notebooks = arr(o.notebooks, "snapshot.notebooks").map((n, i) => notebook(n, `notebook[${i}]`));
  // Same uniqueness scopes as Rust: notebooks, pages and strokes each library-wide.
  const unique = (ids: string[], kind: string) => {
    const seen = new Set<string>();
    for (const id of ids) {
      if (seen.has(id)) fail("snapshot", `duplicate ${kind} ${id}`);
      seen.add(id);
    }
  };
  unique(notebooks.map((n) => n.notebook_id), "notebook");
  unique(notebooks.flatMap((n) => n.pages.map((p) => p.page_id)), "page");
  unique(notebooks.flatMap((n) => n.pages.flatMap((p) => p.strokes.map((s) => s.stroke_id))), "stroke");
  unique(notebooks.flatMap((n) => n.pages.flatMap((p) => p.texts.map((t) => t.text_id))), "text");
  unique(notebooks.flatMap((n) => n.pages.flatMap((p) => p.links.map((l) => l.link_id))), "link");
  for (const n of notebooks) {
    for (const p of n.pages) {
      for (const l of p.links) {
        if (l.source_notebook_id !== n.notebook_id || l.source_page_id !== p.page_id) {
          fail("snapshot", `link ${l.link_id} source does not match page ${n.notebook_id}/${p.page_id}`);
        }
      }
    }
  }
  const folders = "folders" in o ? arr(o.folders, "snapshot.folders").map((f, i) => folder(f, `folder[${i}]`)) : [];
  unique(folders.map((f) => f.folder_id), "folder");
  const live = new Set(folders.map((f) => f.folder_id));
  for (const n of notebooks) {
    if (n.folder_id !== undefined && !live.has(n.folder_id)) fail("snapshot", `notebook ${n.notebook_id} is in unknown folder ${n.folder_id}`);
  }
  return {
    v: 1,
    library_id: uuid(o.library_id, "snapshot.library_id"),
    page_space: { width: CANONICAL_PAGE_WIDTH, height: CANONICAL_PAGE_HEIGHT },
    notebooks,
    folders,
  };
}

/** Full-u64 ledger sequence, parsed losslessly as `bigint` (see `lossless.ts`). */
function seq(v: unknown, where: string): bigint {
  if (typeof v !== "bigint" || v < 0n || v > 18446744073709551615n) fail(where, "expected a u64 sequence");
  return v;
}

function count(v: unknown, where: string): number {
  return int(v, where, 0, Number.MAX_SAFE_INTEGER);
}

export function validateLibraryList(v: unknown): LibrarySummary[] {
  const o = obj(v, "libraries", ["v", "libraries"]);
  if (o.v !== 1) fail("libraries", `unsupported list version ${JSON.stringify(o.v)}`);
  return arr(o.libraries, "libraries.libraries").map((l, i) => {
    const w = `libraries[${i}]`;
    const r = obj(l, w, ["library_id", "head_server_seq", "snapshot_sha256", "notebook_count", "page_count"]);
    return {
      library_id: uuid(r.library_id, `${w}.library_id`),
      head_server_seq: seq(r.head_server_seq, `${w}.head_server_seq`),
      snapshot_sha256: sha256(r.snapshot_sha256, `${w}.snapshot_sha256`),
      notebook_count: count(r.notebook_count, `${w}.notebook_count`),
      page_count: count(r.page_count, `${w}.page_count`),
    };
  });
}

export function validateLibraryHead(v: unknown): LibraryHead {
  const o = obj(v, "library", ["library_id", "genesis_sha256", "head_server_seq", "snapshot_sha256"]);
  return {
    library_id: uuid(o.library_id, "library.library_id"),
    genesis_sha256: sha256(o.genesis_sha256, "library.genesis_sha256"),
    head_server_seq: seq(o.head_server_seq, "library.head_server_seq"),
    snapshot_sha256: sha256(o.snapshot_sha256, "library.snapshot_sha256"),
  };
}

export function validateSnapshotResponse(v: unknown, expectLibrary: string): SnapshotResponse {
  const o = obj(v, "snapshot response", ["library_id", "through_server_seq", "snapshot_sha256", "snapshot"]);
  const libraryId = uuid(o.library_id, "snapshot response.library_id");
  const snapshot = validateSnapshot(o.snapshot);
  if (libraryId !== expectLibrary || snapshot.library_id !== expectLibrary) {
    fail("snapshot response", "library id mismatch");
  }
  return {
    library_id: libraryId,
    through_server_seq: seq(o.through_server_seq, "snapshot response.through_server_seq"),
    snapshot_sha256: sha256(o.snapshot_sha256, "snapshot response.snapshot_sha256"),
    snapshot,
  };
}
