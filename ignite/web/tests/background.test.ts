import { createHash, randomBytes } from "node:crypto";
import { describe, expect, it, vi } from "vitest";
import type { BackgroundRef } from "../src/model/types";
import { validateSnapshot } from "../src/model/validate";
import { BackgroundError, BlobCache, gray8ToRgba, verifyBlob } from "../src/rendering/background";
import { sha256Hex, sha256HexPure } from "../src/rendering/sha256";
import { blobBytes, manifest, snapshotJson } from "./fixtures";

const snap = validateSnapshot(snapshotJson);
const ref = snap.notebooks[1]!.pages[0]!.background as Extract<BackgroundRef, { kind: "blob" }>;

describe("Gray8 → RGBA", () => {
  it("drops stride padding (never shown as pixels)", () => {
    // 3 wide, stride 5, 2 rows: padding bytes are 0x00 and must not appear.
    const src = new Uint8Array([10, 20, 30, 0, 0, 40, 50, 60, 0, 0]);
    const out = gray8ToRgba(src, 3, 2, 5);
    expect(out.length).toBe(3 * 2 * 4);
    expect([...out].filter((_, i) => i % 4 === 0)).toEqual([10, 20, 30, 40, 50, 60]);
    expect([...out].filter((_, i) => i % 4 === 3).every((a) => a === 255)).toBe(true);
  });

  it("packed width == stride", () => {
    const out = gray8ToRgba(new Uint8Array([1, 2, 3, 4]), 2, 2, 2);
    expect([...out].filter((_, i) => i % 4 === 1)).toEqual([1, 2, 3, 4]);
  });

  it("fixture blob: stride > width and the padding column is excluded", () => {
    expect(ref.format.stride).toBeGreaterThan(ref.format.width);
    const rgba = gray8ToRgba(blobBytes, ref.format.width, ref.format.height, ref.format.stride);
    expect(rgba.length).toBe(ref.format.width * ref.format.height * 4);
    // The generator wrote 0x00 into padding bytes; the visible image has no 0x00 pixel.
    expect([...rgba].filter((_, i) => i % 4 === 0).includes(0)).toBe(false);
  });

  it("refuses a too-small buffer", () => {
    expect(() => gray8ToRgba(new Uint8Array(3), 2, 2, 2)).toThrow(BackgroundError);
  });
});

describe("blob verification", () => {
  it("accepts the Rust fixture blob", async () => {
    expect(ref.sha256).toBe(manifest.blob.sha256);
    await verifyBlob(blobBytes, ref);
  });

  it("refuses a SHA-256 mismatch", async () => {
    const tampered = blobBytes.slice();
    tampered[0] = tampered[0]! ^ 0xff;
    await expect(verifyBlob(tampered, ref)).rejects.toThrow(/SHA-256/);
  });

  it("refuses a wrong length", async () => {
    await expect(verifyBlob(blobBytes.slice(1), ref)).rejects.toThrow(/length/);
  });

  it("refuses unsupported encodings / insane formats", async () => {
    await expect(verifyBlob(blobBytes, { ...ref, format: { ...ref.format, encoding: "rgba8" as "gray8" } })).rejects.toThrow(/encoding/);
    await expect(verifyBlob(blobBytes, { ...ref, format: { ...ref.format, stride: 1 } })).rejects.toThrow(/format/);
  });
});

describe("SHA-256", () => {
  it("pure fallback == Web Crypto == node for many sizes", async () => {
    for (const n of [0, 1, 55, 56, 63, 64, 65, 1000, 4096 + 7]) {
      const d = new Uint8Array(randomBytes(n));
      const want = createHash("sha256").update(d).digest("hex");
      expect(sha256HexPure(d)).toBe(want);
      expect(await sha256Hex(d)).toBe(want);
    }
    expect(sha256HexPure(blobBytes)).toBe(manifest.blob.sha256);
  });
});

describe("session blob cache", () => {
  it("fetches, verifies and decodes a blob once per SHA", async () => {
    const fetchBlob = vi.fn(async () => blobBytes);
    const decode = vi.fn(async () => ({}) as CanvasImageSource);
    const cache = new BlobCache(fetchBlob, decode);
    const [a, b] = await Promise.all([cache.load(ref), cache.load(ref)]);
    await cache.load(ref);
    expect(a).toBe(b);
    expect(fetchBlob).toHaveBeenCalledTimes(1);
    expect(decode).toHaveBeenCalledTimes(1);
    expect(cache.get(ref.sha256)).toBe(a);
  });

  it("records a failure instead of caching tampered bytes", async () => {
    const bad = blobBytes.slice();
    bad[5] = bad[5]! ^ 1;
    const cache = new BlobCache(async () => bad, async () => ({}) as CanvasImageSource);
    await expect(cache.load(ref)).rejects.toThrow(/SHA-256/);
    await Promise.resolve();
    expect(cache.get(ref.sha256)).toBeUndefined();
    expect(cache.error(ref.sha256)).toMatch(/SHA-256/);
  });
});
