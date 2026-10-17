// Page backgrounds. `blank` = white paper, no fetch. `blob` = verified durable Gray8 pixels.

import type { BackgroundRef } from "../model/types";
import { sha256Hex } from "./sha256";

export class BackgroundError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BackgroundError";
  }
}

type BlobRef = Extract<BackgroundRef, { kind: "blob" }>;

/** Verify fetched blob bytes against the snapshot's reference (length, SHA-256, format). */
export async function verifyBlob(bytes: Uint8Array, ref: BlobRef): Promise<void> {
  const f = ref.format;
  if (f.encoding !== "gray8") throw new BackgroundError(`unsupported background encoding ${String(f.encoding)}`);
  if (f.width <= 0 || f.height <= 0 || f.stride < f.width || f.stride * f.height !== ref.byte_len) {
    throw new BackgroundError("invalid background format");
  }
  if (bytes.length !== ref.byte_len) {
    throw new BackgroundError(`background length ${bytes.length} != expected ${ref.byte_len}`);
  }
  const got = await sha256Hex(bytes);
  if (got !== ref.sha256) throw new BackgroundError("background SHA-256 does not match the snapshot");
}

/** Gray8 (row `stride`, visible `width`) → RGBA; stride padding is never a pixel. */
export function gray8ToRgba(bytes: Uint8Array, width: number, height: number, stride: number): Uint8ClampedArray {
  if (stride < width || bytes.length < stride * height) throw new BackgroundError("gray8 buffer too small");
  const out = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y++) {
    const row = y * stride;
    for (let x = 0; x < width; x++) {
      const v = bytes[row + x]!;
      const o = (y * width + x) * 4;
      out[o] = v;
      out[o + 1] = v;
      out[o + 2] = v;
      out[o + 3] = 255;
    }
  }
  return out;
}

/** A decoded background ready to draw (browser: canvas/bitmap; tests: any value). */
export interface DecodedBackground {
  width: number;
  height: number;
  image: CanvasImageSource;
}

/**
 * Session cache of verified immutable blobs, keyed by SHA-256 (content-addressed, so a key never
 * changes meaning). In-memory only: no persistent/offline cache in S3A.
 */
export class BlobCache {
  private done = new Map<string, DecodedBackground>();
  private pending = new Map<string, Promise<DecodedBackground>>();
  private failed = new Map<string, string>();

  constructor(
    private readonly fetchBlob: (sha: string) => Promise<Uint8Array>,
    private readonly decode: (rgba: Uint8ClampedArray, width: number, height: number) => Promise<CanvasImageSource>,
  ) {}

  get(sha: string): DecodedBackground | undefined {
    return this.done.get(sha);
  }

  /** Why the last load of `sha` failed (shown as a page-level error, never as white paper). */
  error(sha: string): string | undefined {
    return this.failed.get(sha);
  }

  load(ref: BlobRef): Promise<DecodedBackground> {
    const hit = this.done.get(ref.sha256);
    if (hit) return Promise.resolve(hit);
    const inflight = this.pending.get(ref.sha256);
    if (inflight) return inflight;
    this.failed.delete(ref.sha256);
    const p = (async () => {
      const bytes = await this.fetchBlob(ref.sha256);
      await verifyBlob(bytes, ref);
      const { width, height, stride } = ref.format;
      const image = await this.decode(gray8ToRgba(bytes, width, height, stride), width, height);
      const d = { width, height, image };
      this.done.set(ref.sha256, d);
      return d;
    })();
    this.pending.set(ref.sha256, p);
    p.catch((e: unknown) => {
      this.failed.set(ref.sha256, e instanceof Error ? e.message : "background failed to load");
    }).finally(() => this.pending.delete(ref.sha256));
    return p;
  }

  clear(): void {
    this.done.clear();
    this.pending.clear();
    this.failed.clear();
  }
}

/** Browser decoder: RGBA → an offscreen canvas usable by `drawImage`. */
export async function decodeToCanvas(rgba: Uint8ClampedArray, width: number, height: number): Promise<CanvasImageSource> {
  const c = document.createElement("canvas");
  c.width = width;
  c.height = height;
  const ctx = c.getContext("2d");
  if (!ctx) throw new BackgroundError("canvas 2D unavailable");
  ctx.putImageData(new ImageData(rgba as Uint8ClampedArray<ArrayBuffer>, width, height), 0, 0);
  return c;
}
