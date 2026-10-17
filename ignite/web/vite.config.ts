import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vitest/config";
import react from "@vitejs/plugin-react";

// Same-origin by design: in production the Rust sync server serves `dist/` (`--web-root`) and
// `/v1` from one origin, so no CORS exists anywhere. In development, Vite proxies `/v1` to a
// local sync server instead of the server granting cross-origin access.
const syncServer = process.env.SCRIBE_SYNC_SERVER ?? "http://127.0.0.1:8787";
const here = dirname(fileURLToPath(import.meta.url));
const version = readFileSync(resolve(here, "../VERSION"), "utf8").trim();
const SW_VERSION_PLACEHOLDER = "__SCRIBE_VERSION__";

/** Rewrite the lifecycle worker so a version bump is a byte change the browser can detect. */
function injectSwVersion(): Plugin {
  const srcPath = resolve(here, "public/sw.js");
  const rewrite = (file: string) => {
    if (!existsSync(file)) return;
    const src = readFileSync(file, "utf8");
    if (!src.includes(SW_VERSION_PLACEHOLDER)) return;
    writeFileSync(file, src.replaceAll(SW_VERSION_PLACEHOLDER, version));
  };
  return {
    name: "scribe-sw-version",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const path = req.url?.split("?")[0];
        if (path !== "/sw.js") {
          next();
          return;
        }
        const body = readFileSync(srcPath, "utf8").replaceAll(SW_VERSION_PLACEHOLDER, version);
        res.setHeader("Content-Type", "text/javascript; charset=utf-8");
        res.setHeader("Cache-Control", "no-cache");
        res.end(body);
      });
    },
    writeBundle(options) {
      if (options.dir) rewrite(resolve(options.dir, "sw.js"));
    },
    closeBundle() {
      rewrite(resolve(here, "dist/sw.js"));
    },
  };
}

export default defineConfig({
  plugins: [react(), injectSwVersion()],
  define: {
    __SCRIBE_VERSION__: JSON.stringify(version),
  },
  base: "/",
  server: {
    proxy: {
      "/v1": { target: syncServer, changeOrigin: false },
      "/control": { target: syncServer, changeOrigin: false },
    },
  },
  build: {
    outDir: "dist",
    assetsDir: "assets",
    sourcemap: false,
  },
  test: {
    include: ["tests/**/*.test.ts", "tests/**/*.test.tsx"],
    environment: "node",
  },
});
