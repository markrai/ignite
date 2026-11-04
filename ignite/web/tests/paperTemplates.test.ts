// Slice 6B: Rust/Web Gray8 parity, blob metadata, and publish-before-create.

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import type { BlobApi } from "../src/api/blobApi";
import { TransientError } from "../src/api/replicaApi";
import * as paperTemplates from "../src/paper/paperTemplates";
import {
  PAGE_TEMPLATE_CHOICES,
  SCRIBE_PANEL_HEIGHT,
  SCRIBE_PANEL_WIDTH,
  generatePageBackground,
  generateTemplateBackground,
  type PageTemplate,
} from "../src/paper/paperTemplates";
import { publishTemplateBackground, TemplatePublishError } from "../src/paper/publishTemplateBackground";
import { gray8ToRgba, verifyBlob } from "../src/rendering/background";
import { sha256Hex } from "../src/rendering/sha256";

const FIX = join(import.meta.dirname, "fixtures");
const manifest = JSON.parse(readFileSync(join(FIX, "paper-templates-v1.json"), "utf8")) as {
  v: number;
  width: number;
  height: number;
  stride: number;
  reserved_bottom_rows: number;
  templates: Record<
    PageTemplate,
    {
      sha256: string;
      byte_len: number;
      width: number;
      height: number;
      stride: number;
      encoding: string;
      reserved_bottom_rows: number;
      file: string;
    }
  >;
};

function rustBytes(template: PageTemplate): Uint8Array {
  const entry = manifest.templates[template];
  return new Uint8Array(gunzipSync(readFileSync(join(FIX, entry.file))));
}

describe("paper template geometry (Scribe panel)", () => {
  it("matches Kindle panel constants from Rust fixtures", () => {
    expect(manifest.v).toBe(1);
    expect(manifest.width).toBe(SCRIBE_PANEL_WIDTH);
    expect(manifest.height).toBe(SCRIBE_PANEL_HEIGHT);
    expect(manifest.stride).toBe(SCRIBE_PANEL_WIDTH);
    expect(manifest.reserved_bottom_rows).toBe(57);
    expect(SCRIBE_PANEL_WIDTH * SCRIBE_PANEL_HEIGHT).toBe(1860 * 2480);
  });
});

describe("Rust/Web Gray8 parity", () => {
  it.each(PAGE_TEMPLATE_CHOICES)(
    "%s is byte-identical to the Rust golden (incl. SHA-256)",
    async (template) => {
      const want = rustBytes(template);
      const entry = manifest.templates[template];
      expect(want.length).toBe(entry.byte_len);
      expect(createHash("sha256").update(want).digest("hex")).toBe(entry.sha256);

      const got = generatePageBackground(template);
      expect(got.length).toBe(want.length);
      // Buffer.equals is O(n); Vitest's toEqual on 4.6MiB arrays times out.
      expect(Buffer.from(got.buffer, got.byteOffset, got.byteLength).equals(Buffer.from(want.buffer, want.byteOffset, want.byteLength))).toBe(
        true,
      );
      expect(await sha256Hex(got)).toBe(entry.sha256);
      expect(createHash("sha256").update(got).digest("hex")).toBe(entry.sha256);
    },
    30_000,
  );

  it("shared template backgrounds produce matching SHA references", async () => {
    const a = await generateTemplateBackground("ruled");
    const b = await generateTemplateBackground("ruled");
    expect(a.sha256).toBe(b.sha256);
    expect(a.background).toEqual(b.background);
    expect(a.background).toMatchObject({
      kind: "blob",
      sha256: manifest.templates.ruled.sha256,
      byte_len: manifest.templates.ruled.byte_len,
      format: {
        encoding: "gray8",
        width: SCRIBE_PANEL_WIDTH,
        height: SCRIBE_PANEL_HEIGHT,
        stride: SCRIBE_PANEL_WIDTH,
      },
    });
  });

  it("blank is all white and BackgroundRef::Blank (no blob)", async () => {
    const { bytes, background, sha256 } = await generateTemplateBackground("blank");
    expect(bytes.every((b) => b === 0xff)).toBe(true);
    expect(background).toEqual({ kind: "blank" });
    expect(sha256).toBe(manifest.templates.blank.sha256);
  });
});

describe("blob metadata + nearest-neighbor decode path", () => {
  it.each(["ruled", "grid", "dotted"] as const)("%s verifies and decodes packed Gray8", async (template) => {
    const { bytes, background } = await generateTemplateBackground(template);
    expect(background.kind).toBe("blob");
    if (background.kind !== "blob") return;
    await verifyBlob(bytes, background);
    const rgba = gray8ToRgba(bytes, background.format.width, background.format.height, background.format.stride);
    expect(rgba.length).toBe(background.format.width * background.format.height * 4);
    // Spot-check a marked pixel survives Gray8→RGBA (existing blit uses nearest-neighbor).
    const i = bytes.findIndex((b) => b !== 0xff);
    expect(i).toBeGreaterThanOrEqual(0);
    expect(rgba[i * 4]).toBe(bytes[i]);
    expect(rgba[i * 4 + 3]).toBe(255);
  });
});

describe("publishTemplateBackground", () => {
  it("blank creates without a blob PUT or Gray8 generation", async () => {
    const gen = vi.spyOn(paperTemplates, "generateTemplateBackground");
    const put = vi.fn();
    const api: BlobApi = { put };
    const out = await publishTemplateBackground("blank", api, () => false);
    expect(out.background).toEqual({ kind: "blank" });
    expect(out.sha256).toBeNull();
    expect(put).not.toHaveBeenCalled();
    expect(gen).not.toHaveBeenCalled();
    gen.mockRestore();
  });

  it(
    "patterned uploads the blob before returning a Blob ref",
    async () => {
      const put = vi.fn(async (sha: string, bytes: Uint8Array) => ({
        sha256: sha,
        byte_len: bytes.length,
        created: true,
      }));
      const api: BlobApi = { put };
      const out = await publishTemplateBackground("grid", api, () => true);
      expect(put).toHaveBeenCalledTimes(1);
      expect(out.background.kind).toBe("blob");
      if (out.background.kind !== "blob") return;
      expect(put.mock.calls[0]![0]).toBe(out.background.sha256);
      const uploaded = put.mock.calls[0]![1] as Uint8Array;
      const expected = generatePageBackground("grid");
      expect(
        Buffer.from(uploaded.buffer, uploaded.byteOffset, uploaded.byteLength).equals(
          Buffer.from(expected.buffer, expected.byteOffset, expected.byteLength),
        ),
      ).toBe(true);
      expect(out.sha256).toBe(manifest.templates.grid.sha256);
    },
    30_000,
  );

  it("upload failure does not return a usable notebook background", async () => {
    const put = vi.fn(async () => {
      throw new TransientError(0, "unreachable");
    });
    const api: BlobApi = { put };
    await expect(publishTemplateBackground("dotted", api, () => true)).rejects.toBeInstanceOf(TemplatePublishError);
    expect(put).toHaveBeenCalledTimes(1);
  });

  it("offline patterned creation fails clearly without PUT", async () => {
    const put = vi.fn();
    const api: BlobApi = { put };
    await expect(publishTemplateBackground("ruled", api, () => false)).rejects.toThrow(/network connection/);
    expect(put).not.toHaveBeenCalled();
  });
});
