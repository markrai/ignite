import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  ROUGH_BUTTON_FRAME_CLASS,
  ROUGH_BUTTON_FRAME_POINTER_EVENTS,
  roughCapsulePath,
  seedFromIdentity,
} from "../src/components/RoughControlButton";

const css = readFileSync(join(fileURLToPath(new URL(".", import.meta.url)), "../src/styles.css"), "utf8");

describe("seedFromIdentity", () => {
  it("is stable, in Rough's seed range, and never zero", () => {
    const id = "insert-page";
    const seed = seedFromIdentity(id);
    expect(seed).toBe(seedFromIdentity(id));
    expect(seed).toBeGreaterThanOrEqual(1);
    expect(seed).toBeLessThan(2 ** 31);
    expect(seedFromIdentity("delete-page")).not.toBe(seed);
  });
});

describe("roughCapsulePath", () => {
  it("returns the same path for the same width, height, and seed", () => {
    const a = roughCapsulePath(88, 44, 42);
    const b = roughCapsulePath(88, 44, 42);
    expect(a).toBe(b);
    expect(a.length).toBeGreaterThan(0);
  });

  it("changes when the button dimensions change", () => {
    expect(roughCapsulePath(88, 44, 42)).not.toBe(roughCapsulePath(120, 44, 42));
    expect(roughCapsulePath(44, 44, 42)).not.toBe(roughCapsulePath(44, 48, 42));
  });
});

describe("pointer-events contract", () => {
  it("cannot consume pointer events", () => {
    expect(ROUGH_BUTTON_FRAME_POINTER_EVENTS).toBe("none");
    expect(css).toMatch(
      new RegExp(`\\.${ROUGH_BUTTON_FRAME_CLASS}\\s*\\{[^}]*pointer-events:\\s*none`),
    );
  });
});
