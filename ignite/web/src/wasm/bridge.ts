// S3B browser ↔ Rust protocol bridge (`native/scribe-web-wasm`). All canonical envelope bytes
// and all IDs come from authoritative Rust code; TypeScript never re-implements canonical JSON
// or u64 sequencing, and never re-implements erase geometry: the bridge builds AddStroke and
// EraseRegion envelopes and runs the reducer's own erase for local projection.

import type { SnapshotFolder, SnapshotNotebook, SnapshotPage, SyncSnapshotV1, SyncStroke, SyncText } from "../model/types";
import type { FolderIntent, StructureIntent } from "../replica/store";

export type IdKind = "replica" | "operation" | "stroke" | "folder" | "text" | "notebook" | "page";
export type BridgeStyle = "pressure-width-v1" | "legacy-fixed-v1";

export interface BridgePoint {
  x: number;
  y: number;
  pressure?: number;
}

export interface AddStrokeRequest {
  library_id: string;
  replica_id: string;
  /** Full-u64 decimal/SEQ20 text — never a JS number. */
  client_seq: string;
  operation_id: string;
  notebook_id: string;
  page_id: string;
  stroke_id: string;
  style: BridgeStyle;
  points: BridgePoint[];
}

interface Exports {
  memory: WebAssembly.Memory;
  swa_alloc(len: number): number;
  swa_free(ptr: number, len: number): void;
  swa_call(ptr: number, len: number): bigint;
}

export class BridgeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BridgeError";
  }
}

export class ProtocolBridge {
  private readonly enc = new TextEncoder();
  private readonly dec = new TextDecoder();

  constructor(private readonly x: Exports) {}

  static async instantiate(source: BufferSource | Response | Promise<Response>): Promise<ProtocolBridge> {
    if (!(source instanceof Response || source instanceof Promise)) {
      const { instance } = await WebAssembly.instantiate(source, {});
      return new ProtocolBridge(instance.exports as unknown as Exports);
    }
    const resp = await source;
    if (typeof WebAssembly.instantiateStreaming === "function") {
      try {
        const { instance } = await WebAssembly.instantiateStreaming(resp.clone(), {});
        return new ProtocolBridge(instance.exports as unknown as Exports);
      } catch {
        // MIME / streaming only. A CSP that blocks WebAssembly still fails on instantiate(arrayBuffer).
      }
    }
    const { instance } = await WebAssembly.instantiate(await resp.arrayBuffer(), {});
    return new ProtocolBridge(instance.exports as unknown as Exports);
  }

  /** Raw JSON-in/JSON-out call (tests use it to replay the Rust fixture). */
  call(request: unknown): { ok?: string; err?: string } {
    const bytes = this.enc.encode(JSON.stringify(request));
    const inPtr = this.x.swa_alloc(bytes.length);
    new Uint8Array(this.x.memory.buffer, inPtr, bytes.length).set(bytes);
    const packed = this.x.swa_call(inPtr, bytes.length);
    this.x.swa_free(inPtr, bytes.length);
    const outPtr = Number(packed >> 32n);
    const outLen = Number(packed & 0xffffffffn);
    const text = this.dec.decode(new Uint8Array(this.x.memory.buffer, outPtr, outLen).slice());
    this.x.swa_free(outPtr, outLen);
    return JSON.parse(text) as { ok?: string; err?: string };
  }

  private must(request: unknown): string {
    const r = this.call(request);
    if (typeof r.ok !== "string") throw new BridgeError(r.err ?? "bridge returned no result");
    return r.ok;
  }

  /** A fresh UUIDv7 identity (browser CSPRNG + time), validated by the Rust S1 constructors. */
  mintId(kind: IdKind, random: (a: Uint8Array<ArrayBuffer>) => void = (a) => void crypto.getRandomValues(a), now: () => number = Date.now): string {
    const r = new Uint8Array(10);
    random(r);
    const hex = Array.from(r, (b) => b.toString(16).padStart(2, "0")).join("");
    return this.must({ fn: "mint_id", kind, unix_ms: String(Math.floor(now())), random_hex: hex });
  }

  /** Exact canonical SyncEnvelopeV1 text (AddStroke only) — store and resend verbatim. */
  encodeAddStroke(req: AddStrokeRequest): string {
    return this.must({
      fn: "encode_add_stroke",
      ...req,
      // f32 values: the Rust side parses these decimals exactly back to the same f32.
      points: req.points.map((p) =>
        p.pressure === undefined
          ? { x: Math.fround(p.x), y: Math.fround(p.y) }
          : { x: Math.fround(p.x), y: Math.fround(p.y), pressure: Math.fround(p.pressure) },
      ),
    });
  }

  /**
   * Exact canonical `EraseRegion` text. Rust sets the radius (the Kindle eraser's) and sorts /
   * de-duplicates `observed_roots`; path samples carry no pressure.
   */
  encodeEraseRegion(req: EraseRegionRequest): string {
    return this.must({
      fn: "encode_erase_region",
      ...req,
      path: req.path.map((p) => ({ x: Math.fround(p.x), y: Math.fround(p.y) })),
    });
  }

  /** Exact canonical text of one folder operation (create/rename/delete/set_notebook_folder). */
  encodeFolderOp(req: FolderOpRequest): string {
    return this.must({ fn: "encode_folder_op", ...req });
  }

  /**
   * Folders + memberships after pending folder `ops`, by the reducer's own folder rules
   * (`sync_engine::FolderProjection`). An op the reducer would refuse is skipped.
   */
  projectFolders(folders: readonly SnapshotFolder[], notebooks: readonly Membership[], ops: readonly FolderIntent[]): FolderProjection {
    const out = this.must({ fn: "project_folders", folders, notebooks, ops });
    return JSON.parse(out) as FolderProjection;
  }

  /**
   * One page's visible strokes after `ops`, computed by the reducer's own erase
   * (`sync_engine::erase_page_strokes`) and lineage delete. `changed[i]`: whether op `i` altered the page.
   */
  projectPage(strokes: readonly SyncStroke[], ops: readonly ProjectOp[]): { strokes: SyncStroke[]; changed: boolean[] } {
    const out = this.must({
      fn: "project_page",
      strokes,
      ops: ops.map((o) => {
        if (o.op === "add") return o;
        if (o.op === "delete_lineage") return o;
        return { op: "erase", operation_id: o.operation_id, observed_roots: o.observed_roots, path: o.path.map((p) => ({ x: p.x, y: p.y })) };
      }),
    });
    return JSON.parse(out) as { strokes: SyncStroke[]; changed: boolean[] };
  }

  encodeStructureOp(req: StructureOpRequest): string {
    return this.must({ fn: "encode_structure_op", ...req });
  }

  encodeDeleteStrokeLineage(req: DeleteStrokeLineageRequest): string {
    return this.must({ fn: "encode_delete_stroke_lineage", ...req });
  }

  /**
   * Notebooks, pages, folders and membership after pending library-scoped `ops` (client_seq order),
   * via `LibraryState`. An op the reducer would refuse is skipped.
   */
  projectLibrary(snapshot: SyncSnapshotV1, ops: readonly LibraryOp[]): LibraryProjection {
    const out = this.must({ fn: "project_library", snapshot, ops });
    return normalizeLibraryProjection(JSON.parse(out) as LibraryProjection);
  }

  /** Sealed Kindle v2-pressure render profile for a new notebook's first blank page. */
  createNotebookRender(): import("../model/types").RenderProfileRef {
    return JSON.parse(this.must({ fn: "create_notebook_render" })) as import("../model/types").RenderProfileRef;
  }

  encodeTextOp(fn: "encode_add_text" | "encode_update_text", req: TextOpRequest): string {
    const t = req.text;
    return this.must({
      fn,
      library_id: req.library_id,
      replica_id: req.replica_id,
      client_seq: req.client_seq,
      operation_id: req.operation_id,
      notebook_id: req.notebook_id,
      page_id: req.page_id,
      text: {
        ...t,
        x: Math.fround(t.x),
        y: Math.fround(t.y),
        width: Math.fround(t.width),
        font_size: Math.fround(t.font_size),
      },
    });
  }

  encodeDeleteText(req: DeleteTextRequest): string {
    return this.must({ fn: "encode_delete_text", ...req });
  }

  projectTexts(texts: readonly SyncText[], ops: readonly TextProjectOp[]): { texts: SyncText[]; changed: boolean[] } {
    const out = this.must({ fn: "project_texts", texts, ops });
    return JSON.parse(out) as { texts: SyncText[]; changed: boolean[] };
  }
}

export interface EraseRegionRequest {
  library_id: string;
  replica_id: string;
  /** Full-u64 decimal/SEQ20 text — never a JS number. */
  client_seq: string;
  operation_id: string;
  notebook_id: string;
  page_id: string;
  observed_roots: readonly string[];
  path: readonly { x: number; y: number }[];
}

export interface FolderOpRequest {
  library_id: string;
  replica_id: string;
  /** Full-u64 decimal/SEQ20 text — never a JS number. */
  client_seq: string;
  operation_id: string;
  operation: FolderIntent;
}

export interface Membership {
  notebook_id: string;
  folder_id?: string;
}

export interface FolderProjection {
  folders: SnapshotFolder[];
  notebooks: Membership[];
  changed: boolean[];
}

export interface StructureOpRequest {
  library_id: string;
  replica_id: string;
  client_seq: string;
  operation_id: string;
  operation: StructureIntent;
}

export interface DeleteStrokeLineageRequest {
  library_id: string;
  replica_id: string;
  client_seq: string;
  operation_id: string;
  notebook_id: string;
  page_id: string;
  root_id: string;
}

export type LibraryOp = FolderIntent | StructureIntent;

export interface LibraryProjection {
  notebooks: SnapshotNotebook[];
  folders: SnapshotFolder[];
  changed: boolean[];
}

/**
 * WASM `project_library` serializes a snapshot with the same omit-empty rules as the wire
 * (`texts` / `links` / `folders` absent when empty). Validated server snapshots always fill
 * those as `[]`. Callers (sidebar, PageView) treat them as arrays, so the projection boundary
 * matches `validateSnapshot` rather than leaving a just-created blank page with `links: undefined`.
 */
export function normalizeLibraryProjection(raw: LibraryProjection): LibraryProjection {
  return {
    notebooks: raw.notebooks.map((n) => ({
      ...n,
      pages: n.pages.map(
        (p): SnapshotPage => ({
          ...p,
          texts: p.texts ?? [],
          links: p.links ?? [],
        }),
      ),
    })),
    folders: raw.folders ?? [],
    changed: raw.changed,
  };
}

export type ProjectOp =
  | { op: "add"; stroke: SyncStroke }
  | { op: "erase"; operation_id: string; observed_roots: readonly string[]; path: readonly { x: number; y: number }[] }
  | { op: "delete_lineage"; root_id: string };

export interface TextOpRequest {
  library_id: string;
  replica_id: string;
  client_seq: string;
  operation_id: string;
  notebook_id: string;
  page_id: string;
  text: SyncText;
}

export interface DeleteTextRequest {
  library_id: string;
  replica_id: string;
  client_seq: string;
  operation_id: string;
  notebook_id: string;
  page_id: string;
  text_id: string;
}

export type TextProjectOp =
  | { op: "add"; text: SyncText }
  | { op: "update"; text: SyncText }
  | { op: "delete"; text_id: string };

let shared: Promise<ProtocolBridge> | null = null;

/** Browser singleton: the production build serves the `.wasm` as a hashed asset. */
export function loadBridge(): Promise<ProtocolBridge> {
  if (!shared) {
    shared = import("./scribe_web_wasm.wasm?url")
      .then((m: { default: string }) => ProtocolBridge.instantiate(fetch(m.default, { credentials: "same-origin" })))
      .catch((e) => {
        shared = null;
        throw e;
      });
  }
  return shared;
}
