#!/usr/bin/env node
/** Verify dist/ WASM is byte-identical to the Cargo release artifact. */
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  resolveCargoTargetDir,
  resolveCargoWasmArtifact,
  resolveRepoRoot,
  verifyDistWasmMatches,
} from "./wasm-build.mjs";

const webDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
try {
  const repoRoot = resolveRepoRoot(webDir);
  const targetDir = resolveCargoTargetDir(process.env);
  const cargoWasm = resolveCargoWasmArtifact(repoRoot, targetDir);
  const verified = verifyDistWasmMatches(cargoWasm, resolve(webDir, "dist"));
  console.log(`dist WASM matches Cargo artifact (sha256=${verified.sha256})`);
} catch (e) {
  console.error(e instanceof Error ? e.message : e);
  process.exit(1);
}
