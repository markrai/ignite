// Phase 2 install metadata: Web App Manifest, icons, and a lifecycle-only /sw.js.
// The worker is not an offline shell — no fetch handler, no Cache Storage.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const root = join(fileURLToPath(new URL(".", import.meta.url)), "..");
const repoRoot = join(root, "..");
const pub = join(root, "public");
const dist = join(root, "dist");
const productVersion = readFileSync(join(repoRoot, "VERSION"), "utf8").trim();

/** Stable PWA identity — must never embed the product version. */
const PWA_ID = "/ignite-web";

function loadManifest(dir: string) {
  const text = readFileSync(join(dir, "manifest.webmanifest"), "utf8");
  return { text, json: JSON.parse(text) as {
    name: string;
    short_name: string;
    id?: string;
    start_url: string;
    scope: string;
    display: string;
    display_override?: string[];
    background_color: string;
    theme_color: string;
    icons: { src: string; sizes: string; type: string; purpose: string }[];
  } };
}

function resolvePublic(src: string): string {
  return join(pub, src.replace(/^\//, ""));
}

describe("Web App Manifest (Phase 2 install metadata)", () => {
  const { json: m } = loadManifest(pub);

  it("has intentional identity and launch fields", () => {
    expect(m.name).toBe("ignite-web");
    expect(m.short_name).toBe("ignite-web");
    expect(m.start_url).toBe("/");
    expect(m.id).toBe(PWA_ID);
    expect(m.scope).toBe("/");
    expect(m.display).toBe("fullscreen");
    expect(m.display_override).toEqual(["fullscreen", "standalone"]);
    expect(m.background_color).toBe("#f4f2ee");
    expect(m.theme_color).toBe("#f4f2ee");
  });

  it("keeps PWA id /ignite-web independent of product version", () => {
    expect(m.id).toBe(PWA_ID);
    expect(m.id).not.toContain(productVersion);
    expect(m.id).not.toMatch(/\d+\.\d+/);
    // Version still lives in the product tree for app/SW display — not in the PWA id.
    expect(productVersion.length).toBeGreaterThan(0);
    expect(productVersion).toMatch(/^\d+\.\d+/);
  });

  it("does not encode hosts, tokens, or library ids", () => {
    const blob = JSON.stringify(m);
    expect(blob).not.toMatch(/https?:\/\//i);
    expect(blob).not.toMatch(/token|library_id|Bearer/i);
    expect(m.start_url).not.toContain("?");
  });

  it("lists 192, 512, and a separate maskable icon that all exist", () => {
    const sizes = new Set(m.icons.map((i) => `${i.sizes}:${i.purpose}`));
    expect(sizes.has("192x192:any")).toBe(true);
    expect(sizes.has("512x512:any")).toBe(true);
    expect(sizes.has("512x512:maskable")).toBe(true);
    for (const icon of m.icons) {
      expect(icon.type).toBe("image/png");
      expect(existsSync(resolvePublic(icon.src)), icon.src).toBe(true);
    }
  });

  it("index.html references the manifest, apple touch icon, and boot.css", () => {
    const html = readFileSync(join(root, "index.html"), "utf8");
    expect(html).toContain('rel="manifest"');
    expect(html).toContain("/manifest.webmanifest");
    expect(html).toContain('rel="apple-touch-icon"');
    expect(html).toContain("/icons/apple-touch-icon.png");
    expect(html).toContain("/boot.css");
    expect(html).toContain("apple-mobile-web-app-capable");
    expect(html).toContain('name="theme-color"');
    expect(html).toContain('content="ignite-web"');
    expect(html).toContain("<title>ignite-web</title>");
    expect(html).not.toMatch(/serviceWorker|navigator\.serviceWorker/i);
  });

  it("ships a lifecycle-only service worker with no fetch or Cache Storage", () => {
    const sw = readFileSync(join(pub, "sw.js"), "utf8");
    expect(sw).toContain("__SCRIBE_VERSION__");
    expect(sw).toContain("SKIP_WAITING");
    expect(sw).toContain("skipWaiting");
    expect(sw).toContain("clients.claim");
    expect(sw).not.toMatch(/\bfetch\s*\(|addEventListener\(\s*["']fetch["']/);
    expect(sw).not.toMatch(/\bcaches\s*\.\s*open|CacheStorage/);
  });

  it("apple-touch and favicon files exist", () => {
    expect(existsSync(join(pub, "icons/apple-touch-icon.png"))).toBe(true);
    expect(existsSync(join(pub, "icons/favicon-32.png"))).toBe(true);
    expect(existsSync(join(pub, "boot.css"))).toBe(true);
  });

  it("production dist includes manifest and icons when built", () => {
    if (!existsSync(join(dist, "index.html"))) {
      // Unit suite alone does not require a prior build.
      return;
    }
    const html = readFileSync(join(dist, "index.html"), "utf8");
    expect(html).toContain("/manifest.webmanifest");
    expect(existsSync(join(dist, "manifest.webmanifest"))).toBe(true);
    const built = loadManifest(dist).json;
    expect(built.name).toBe("ignite-web");
    expect(built.short_name).toBe("ignite-web");
    expect(built.id).toBe(PWA_ID);
    expect(built.id).not.toContain(productVersion);
    for (const icon of built.icons) {
      expect(existsSync(join(dist, icon.src.replace(/^\//, ""))), icon.src).toBe(true);
    }
    expect(existsSync(join(dist, "icons/apple-touch-icon.png"))).toBe(true);
    expect(existsSync(join(dist, "boot.css"))).toBe(true);
    expect(existsSync(join(dist, "sw.js"))).toBe(true);
    const sw = readFileSync(join(dist, "sw.js"), "utf8");
    expect(sw).toContain(productVersion);
    expect(sw).not.toContain("__SCRIBE_VERSION__");
    expect(sw).not.toMatch(/\bfetch\s*\(|addEventListener\(\s*["']fetch["']/);
    expect(sw).not.toMatch(/\bcaches\s*\.\s*open/);
  });
});
