#!/usr/bin/env node
/**
 * Test-only Cargo stand-in. Writes a deterministic fake WASM when invoked with
 * the same flags the production builder uses. Never used by production CLIs —
 * injected only via in-process `cargoRunner` in unit tests.
 *
 * Env:
 *   MOCK_CARGO_FAIL=1     — exit 1 without writing
 *   MOCK_CARGO_PAYLOAD    — utf8 payload body (default: mock-wasm-v1)
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const args = process.argv.slice(2);
const want = [
  "build",
  "--release",
  "--locked",
  "--target",
  "wasm32-unknown-unknown",
  "-p",
  "scribe-web-wasm",
];
for (let i = 0; i < want.length; i++) {
  if (args[i] !== want[i]) {
    console.error(`mock-cargo: unexpected args: ${args.join(" ")}`);
    process.exit(2);
  }
}

if (process.env.MOCK_CARGO_FAIL === "1") {
  console.error("mock-cargo: simulated failure");
  process.exit(1);
}

const targetDir = process.env.CARGO_TARGET_DIR;
if (!targetDir) {
  console.error("mock-cargo: CARGO_TARGET_DIR required in tests");
  process.exit(2);
}

const out = join(targetDir, "wasm32-unknown-unknown", "release", "scribe_web_wasm.wasm");
mkdirSync(dirname(out), { recursive: true });
const payload = process.env.MOCK_CARGO_PAYLOAD ?? "mock-wasm-v1";
writeFileSync(out, Buffer.from(`\0asm\x01\x00\x00\x00${payload}`));
process.exit(0);
