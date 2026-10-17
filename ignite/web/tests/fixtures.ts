// Loads the committed Rust-generated fixtures (see native/scribe-sync-tool/tests/web_fixtures.rs).
import { readFileSync } from "node:fs";
import { join } from "node:path";

const dir = join(import.meta.dirname, "fixtures");

export interface FixtureManifest {
  snapshot_sha256: string;
  library_id: string;
  blob: { sha256: string; file: string };
  snapshot_sha256_after_live_ops: string[];
}

export const manifest: FixtureManifest = JSON.parse(readFileSync(join(dir, "fixture-manifest.json"), "utf8"));
export const snapshotBytes = new Uint8Array(readFileSync(join(dir, "snapshot-v1.json")));
export const snapshotJson: unknown = JSON.parse(new TextDecoder().decode(snapshotBytes));
export const blobBytes = new Uint8Array(readFileSync(join(dir, manifest.blob.file)));
export const brush = JSON.parse(readFileSync(join(dir, "brush-v1.json"), "utf8")) as {
  curve: {
    q_r1_max: number;
    q_r2_max: number;
    q_r3_max: number;
    min_radius: number;
    max_radius: number;
    missing_fallback_radius: number;
  };
  legacy_fixed_radius: number;
  pressure: { p: number | null; q: number | null; radius: number }[];
  panel_pixel: { x: number; y: number; px: number; py: number }[];
  segments: {
    from: { x: number; y: number; p: number | null };
    to: { x: number; y: number; p: number | null };
    from_px: [number, number];
    to_px: [number, number];
    steps: number;
    radii: number[];
  }[];
};

export function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v)) as T;
}
