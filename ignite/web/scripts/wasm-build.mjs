#!/usr/bin/env node
/**
 * Authoritative ignite-web WASM preparation.
 *
 * Builds `scribe-web-wasm` from the Ignite Cargo workspace (`--release --locked
 * --target wasm32-unknown-unknown`) and copies the artifact to
 * `src/wasm/scribe_web_wasm.wasm`. Fails closed — never treats a pre-existing
 * browser WASM file as success after a Cargo failure.
 */

import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
const WASM_CRATE = "scribe-web-wasm";
const WASM_ARTIFACT = "scribe_web_wasm.wasm";
const WASM_TARGET = "wasm32-unknown-unknown";

/** @param {string} webDir Absolute path to ignite/web */
export function resolveRepoRoot(webDir) {
  // ignite/web -> ignite -> repository root (Cargo.toml workspace)
  const root = resolve(webDir, "..", "..");
  const cargoToml = join(root, "Cargo.toml");
  if (!existsSync(cargoToml)) {
    throw new Error(`REFUSE: Cargo workspace not found at ${cargoToml}`);
  }
  const text = readFileSync(cargoToml, "utf8");
  if (!text.includes("scribe-web-wasm")) {
    throw new Error(`REFUSE: ${cargoToml} does not list scribe-web-wasm`);
  }
  return root;
}

/** @param {NodeJS.ProcessEnv} [env] */
export function resolveCargoTargetDir(env = process.env) {
  const override = env.CARGO_TARGET_DIR;
  if (override && override.trim()) {
    return resolve(override.trim());
  }
  return null; // caller joins with repoRoot/target
}

/**
 * @param {string} repoRoot
 * @param {string | null} targetDir Absolute target dir, or null for <repo>/target
 */
export function resolveCargoWasmArtifact(repoRoot, targetDir) {
  const base = targetDir ?? join(repoRoot, "target");
  return join(base, WASM_TARGET, "release", WASM_ARTIFACT);
}

/** @param {string} webDir */
export function resolveBrowserWasmPath(webDir) {
  return join(webDir, "src", "wasm", WASM_ARTIFACT);
}

/** @param {string} filePath */
export function sha256File(filePath) {
  const buf = readFileSync(filePath);
  return createHash("sha256").update(buf).digest("hex");
}

/**
 * Locate emitted WASM assets under a Vite dist directory.
 * @param {string} distDir
 * @returns {string[]} absolute paths
 */
export function findEmittedWasmAssets(distDir) {
  if (!existsSync(distDir)) {
    throw new Error(`REFUSE: dist directory missing: ${distDir}`);
  }
  /** @type {string[]} */
  const found = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const p = join(dir, name);
      const st = statSync(p);
      if (st.isDirectory()) walk(p);
      else if (name.endsWith(".wasm")) found.push(p);
    }
  };
  walk(distDir);
  return found;
}

/**
 * Exactly one fingerprinted WASM must exist, and it must match the Cargo bytes.
 * @param {string} cargoWasmPath
 * @param {string} distDir
 */
export function verifyDistWasmMatches(cargoWasmPath, distDir) {
  if (!existsSync(cargoWasmPath)) {
    throw new Error(`REFUSE: Cargo WASM missing for verification: ${cargoWasmPath}`);
  }
  const emitted = findEmittedWasmAssets(distDir);
  if (emitted.length === 0) {
    throw new Error(`REFUSE: no .wasm asset under ${distDir}`);
  }
  if (emitted.length > 1) {
    throw new Error(
      `REFUSE: ambiguous WASM assets under ${distDir}: ${emitted.join(", ")}`,
    );
  }
  const expected = sha256File(cargoWasmPath);
  const actual = sha256File(emitted[0]);
  if (expected !== actual) {
    throw new Error(
      `REFUSE: dist WASM SHA-256 mismatch\n  cargo: ${expected}\n  dist:  ${actual}\n  file:  ${emitted[0]}`,
    );
  }
  return { path: emitted[0], sha256: actual };
}

/**
 * @param {{
 *   repoRoot: string,
 *   cargoBin?: string,
 *   cargoRunner?: { command: string, argsPrefix?: string[] },
 *   targetDir?: string | null,
 *   env?: NodeJS.ProcessEnv,
 * }} opts
 */
export function runCargoWasmBuild(opts) {
  const env = { ...process.env, ...(opts.env ?? {}) };
  const targetDir =
    opts.targetDir !== undefined ? opts.targetDir : resolveCargoTargetDir(env);
  if (targetDir) {
    env.CARGO_TARGET_DIR = targetDir;
  }

  // Production CLIs never pass cargoRunner. Tests may inject one in-process.
  // There is intentionally no environment override that can replace Cargo —
  // that would allow shipping synthetic WASM bytes.
  /** @type {{ command: string, argsPrefix: string[] }} */
  const runner = opts.cargoRunner
    ? {
        command: opts.cargoRunner.command,
        argsPrefix: opts.cargoRunner.argsPrefix ?? [],
      }
    : { command: opts.cargoBin ?? "cargo", argsPrefix: [] };

  const cargoArgs = [
    ...runner.argsPrefix,
    "build",
    "--release",
    "--locked",
    "--target",
    WASM_TARGET,
    "-p",
    WASM_CRATE,
  ];

  const result = spawnSync(runner.command, cargoArgs, {
    cwd: opts.repoRoot,
    env,
    encoding: "utf8",
    shell: false,
    stdio: ["ignore", "pipe", "pipe"],
  });

  if (result.error) {
    throw new Error(
      `REFUSE: failed to spawn Cargo (${runner.command}): ${result.error.message}`,
    );
  }
  if (result.status !== 0) {
    const detail = [result.stdout, result.stderr].filter(Boolean).join("\n").trim();
    throw new Error(
      `REFUSE: Cargo build of ${WASM_CRATE} failed (exit ${result.status})${detail ? `\n${detail}` : ""}`,
    );
  }

  const artifact = resolveCargoWasmArtifact(opts.repoRoot, targetDir);
  if (!existsSync(artifact)) {
    throw new Error(`REFUSE: Cargo succeeded but WASM artifact missing: ${artifact}`);
  }
  const size = statSync(artifact).size;
  if (size <= 0) {
    throw new Error(`REFUSE: Cargo WASM artifact is empty: ${artifact}`);
  }
  return { artifact, targetDir, sha256: sha256File(artifact), size };
}

/**
 * Copy only after Cargo succeeds. Uses a temp file then rename.
 * @param {string} from
 * @param {string} to
 */
export function copyWasmArtifact(from, to) {
  mkdirSync(dirname(to), { recursive: true });
  const tmp = `${to}.${process.pid}.${Date.now()}.tmp`;
  try {
    copyFileSync(from, tmp);
    const fromHash = sha256File(from);
    if (sha256File(tmp) !== fromHash) {
      throw new Error("REFUSE: WASM copy corrupted before rename");
    }
    renameSync(tmp, to);
    if (sha256File(to) !== fromHash) {
      throw new Error("REFUSE: WASM destination hash mismatch after copy");
    }
    return fromHash;
  } catch (e) {
    try {
      if (existsSync(tmp)) unlinkSync(tmp);
    } catch {
      /* ignore */
    }
    throw e;
  }
}

/**
 * Build via Cargo and install into the browser source tree.
 * @param {{
 *   webDir: string,
 *   browserWasmPath?: string,
 *   cargoBin?: string,
 *   cargoRunner?: { command: string, argsPrefix?: string[] },
 *   targetDir?: string | null,
 *   env?: NodeJS.ProcessEnv,
 *   runCargo?: typeof runCargoWasmBuild,
 * }} opts
 */
export function prepareBrowserWasm(opts) {
  const webDir = resolve(opts.webDir);
  const repoRoot = resolveRepoRoot(webDir);
  const browserWasm = opts.browserWasmPath ?? resolveBrowserWasmPath(webDir);
  const runCargo = opts.runCargo ?? runCargoWasmBuild;

  const built = runCargo({
    repoRoot,
    cargoBin: opts.cargoBin,
    cargoRunner: opts.cargoRunner,
    targetDir: opts.targetDir,
    env: opts.env,
  });

  const sha256 = copyWasmArtifact(built.artifact, browserWasm);
  return {
    repoRoot,
    cargoArtifact: built.artifact,
    browserWasm,
    sha256,
    size: built.size,
  };
}
