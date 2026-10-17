#!/usr/bin/env node
/**
 * Production build for ignite-web:
 *   1) Cargo-build + install WASM (fail closed)
 *   2) TypeScript check
 *   3) Vite production bundle
 *   4) SHA-256 verify dist WASM == Cargo artifact
 */

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  prepareBrowserWasm,
  verifyDistWasmMatches,
} from "./wasm-build.mjs";

const webDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/** @param {string} relFromNodeModules @param {string[]} args */
function runNodeBin(relFromNodeModules, args) {
  const bin = join(webDir, "node_modules", relFromNodeModules);
  if (!existsSync(bin)) {
    throw new Error(`REFUSE: missing local binary ${bin} (run npm ci)`);
  }
  const result = spawnSync(process.execPath, [bin, ...args], {
    cwd: webDir,
    env: process.env,
    encoding: "utf8",
    shell: false,
    stdio: "inherit",
  });
  if (result.error) {
    throw new Error(`REFUSE: failed to run ${relFromNodeModules}: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(`REFUSE: ${relFromNodeModules} failed (exit ${result.status})`);
  }
}

try {
  console.log("ignite-web: building Rust WASM bridge (Cargo --release --locked)…");
  const prepared = prepareBrowserWasm({ webDir });
  console.log(
    `ignite-web: WASM installed (${prepared.size} bytes, sha256=${prepared.sha256})`,
  );

  console.log("ignite-web: typecheck…");
  runNodeBin(join("typescript", "bin", "tsc"), ["--noEmit", "-p", "tsconfig.json"]);

  console.log("ignite-web: vite build…");
  runNodeBin(join("vite", "bin", "vite.js"), ["build"]);

  console.log("ignite-web: verifying dist WASM matches Cargo output…");
  const verified = verifyDistWasmMatches(
    prepared.cargoArtifact,
    resolve(webDir, "dist"),
  );
  console.log(
    `ignite-web: production build OK (dist WASM sha256=${verified.sha256})`,
  );
} catch (e) {
  console.error(e instanceof Error ? e.message : e);
  process.exit(1);
}
