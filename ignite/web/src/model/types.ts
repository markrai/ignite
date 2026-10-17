// Consumer-side DTOs for the S3A read-only viewer.
//
// Rust `native/sync-protocol` is the authoritative wire definition. These types describe only
// the fields ignite-web consumes; `validate.ts` checks every response at runtime (unknown
// fields are refused, like `serde(deny_unknown_fields)`), so a Rust protocol change fails
// loudly instead of rendering something partial.

export type Uuid = string;
export type Sha256Hex = string;

export interface PageSpace {
  width: number;
  height: number;
}

export interface SyncPoint {
  x: number;
  y: number;
  pressure?: number;
}

export type StrokeStyle = "legacy-fixed-v1" | "pressure-width-v1";

export interface SyncStroke {
  stroke_id: Uuid;
  root_id: Uuid;
  style: StrokeStyle;
  points: SyncPoint[];
}

export type TextFace = "default-sans-v1";

export interface SyncText {
  text_id: Uuid;
  x: number;
  y: number;
  width: number;
  font_size: number;
  face: TextFace;
  content: string;
}

/** Explicit spatial page-to-page link. Omitted on the wire when a page has none. */
export interface SyncPageLink {
  link_id: Uuid;
  source_notebook_id: Uuid;
  source_page_id: Uuid;
  source_x: number;
  source_y: number;
  target_notebook_id: Uuid;
  target_page_id: Uuid;
}

export interface BackgroundFormat {
  encoding: "gray8";
  width: number;
  height: number;
  stride: number;
}

export type BackgroundRef =
  | { kind: "blank" }
  | { kind: "blob"; sha256: Sha256Hex; byte_len: number; format: BackgroundFormat };

export interface PressureCurveRef {
  q_r1_max: number;
  q_r2_max: number;
  q_r3_max: number;
  min_radius: number;
  max_radius: number;
  missing_fallback_radius: number;
}

export interface RenderProfileRef {
  profile_version: number;
  ink_byte: number;
  legacy_fixed_radius: number;
  transform_id: string;
  rasterizer_id: string;
  pressure?: PressureCurveRef;
}

export interface SnapshotPage {
  page_id: Uuid;
  background: BackgroundRef;
  render: RenderProfileRef;
  /** Visible strokes in render (bottom-to-top) order. Never reorder. */
  strokes: SyncStroke[];
  /** Positioned text objects; omitted on the wire when empty. */
  texts: SyncText[];
  /** Explicit spatial page-to-page links; omitted on the wire when empty. */
  links: SyncPageLink[];
}

/** A page as created (`CreateNotebook.first_page` / `CreatePage.page`). */
export interface PageSpec {
  page_id: Uuid;
  background: BackgroundRef;
  render: RenderProfileRef;
}

export interface SnapshotNotebook {
  notebook_id: Uuid;
  title: string;
  pages: SnapshotPage[];
  /** The folder holding this notebook; absent = top level. */
  folder_id?: Uuid;
}

/** A library folder (display grouping; creation order). */
export interface SnapshotFolder {
  folder_id: Uuid;
  title: string;
}

export interface SyncSnapshotV1 {
  v: 1;
  library_id: Uuid;
  page_space: PageSpace;
  notebooks: SnapshotNotebook[];
  /** Omitted on the wire when empty; always present (possibly empty) after validation. */
  folders: SnapshotFolder[];
}

/** `GET /v1/libraries` row (no library title exists in the model). */
export interface LibrarySummary {
  library_id: Uuid;
  /** Full u64 (lossless; never a JS number). */
  head_server_seq: bigint;
  snapshot_sha256: Sha256Hex;
  notebook_count: number;
  page_count: number;
}

/** `GET /v1/libraries/{id}` (poll target). */
export interface LibraryHead {
  library_id: Uuid;
  genesis_sha256: Sha256Hex;
  head_server_seq: bigint;
  snapshot_sha256: Sha256Hex;
}

/** `GET /v1/libraries/{id}/snapshot`. */
export interface SnapshotResponse {
  library_id: Uuid;
  through_server_seq: bigint;
  snapshot_sha256: Sha256Hex;
  snapshot: SyncSnapshotV1;
}
