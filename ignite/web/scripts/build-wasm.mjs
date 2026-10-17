#!/usr/bin/env node
/** CLI: Cargo-build scribe-web-wasm and install into src/wasm/. */
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { prepareBrowserWasm } from "./wasm-build.mjs";

const webDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
try {
  const result = prepareBrowserWasm({ webDir });
  console.log(
    `ignite-web WASM ready: ${result.browserWasm} (${result.size} bytes, sha256=${result.sha256})`,
  );
} catch (e) {
  console.error(e instanceof Error ? e.message : e);
  process.exit(1);
}
