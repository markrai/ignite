import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { APP_VERSION, APP_VERSION_LABEL } from "../src/version";

// tests/ → ignite/web → ignite/ (authoritative VERSION lives here)
const ignitePkg = join(fileURLToPath(new URL(".", import.meta.url)), "../..");
const productVersion = readFileSync(join(ignitePkg, "VERSION"), "utf8").trim();

describe("product version", () => {
  it("is injected from the repo-root VERSION file", () => {
    expect(productVersion).toMatch(/^\d+\.\d+\.\d+$/);
    expect(APP_VERSION).toBe(productVersion);
    expect(APP_VERSION_LABEL).toBe(`v${productVersion}`);
  });
});
